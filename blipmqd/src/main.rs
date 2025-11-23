use std::error::Error;
use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use tokio::signal;
use tokio::sync::watch;
use tokio::time::{self, Duration};
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

use auth::StaticApiKeyValidator;
use bytes::Bytes;
use config::{Config, ConfigError};
use corelib::{Broker, BrokerConfig, QoSLevel};
use metrics::run_metrics_server;
use net::{
    AckPayload, BrokerHandler, Frame, FrameResponse, FrameType, MessageHandler, NetworkConfig,
    PollPayload, PublishPayload, Server, SubscribePayload,
};
use wal::{WalConfig, WriteAheadLog};

#[derive(Parser, Debug)]
#[command(name = "blipmqd", about = "BlipMQ broker daemon")]
struct Cli {
    /// Path to configuration file (TOML or YAML).
    #[arg(long)]
    config: Option<String>,
}

fn load_config(path: Option<&str>) -> Result<Config, ConfigError> {
    Config::load(path)
}

async fn run_integration_flow(
    handler: BrokerHandler,
    _broker: Arc<Broker>,
) -> Result<(), Box<dyn Error>> {
    // Simulate a client performing SUBSCRIBE, PUBLISH, POLL, and ACK.
    let conn_id = 1u64;

    // 1) Subscribe.
    let subscribe_payload = SubscribePayload {
        topic: "test".to_string(),
        qos: 1,
    }
    .encode()?;

    let subscribe_frame = Frame {
        msg_type: FrameType::Subscribe,
        correlation_id: 1,
        payload: subscribe_payload,
    };

    let response = handler.handle_frame(conn_id, subscribe_frame).await?;
    let sub_id = match response {
        FrameResponse::Frame(frame) if frame.msg_type == FrameType::Ack => {
            let ack = AckPayload::decode(&frame.payload)?;
            ack.subscription_id
        }
        _ => return Err("expected ACK to SUBSCRIBE".into()),
    };

    // 2) Publish a QoS1 message.
    let publish_payload = PublishPayload {
        topic: "test".to_string(),
        qos: 1,
        message: Bytes::from_static(b"hello"),
    }
    .encode()?;

    let publish_frame = Frame {
        msg_type: FrameType::Publish,
        correlation_id: 2,
        payload: publish_payload,
    };

    let publish_response = handler.handle_frame(conn_id, publish_frame).await?;
    if let FrameResponse::Frame(_) = publish_response {
        return Err("unexpected frame in response to PUBLISH".into());
    }

    // 3) Poll for the message.
    let poll_payload = PollPayload {
        subscription_id: sub_id,
    }
    .encode()?;

    let poll_frame = Frame {
        msg_type: FrameType::Poll,
        correlation_id: 3,
        payload: poll_payload,
    };

    let delivery_frame = handler.handle_frame(conn_id, poll_frame).await?;
    let (delivery_frame, delivery_payload) = match delivery_frame {
        FrameResponse::Frame(frame) if frame.msg_type == FrameType::Publish => {
            let payload = PublishPayload::decode(&frame.payload)?;
            (frame, payload)
        }
        FrameResponse::None => {
            return Err("expected delivered message on POLL".into());
        }
        FrameResponse::Frame(_) => {
            return Err("unexpected frame type on POLL".into());
        }
    };

    if delivery_payload.topic != "test" || delivery_payload.qos != 1 {
        return Err("delivered payload does not match expectations".into());
    }

    // 4) ACK the delivered message using the correlation id as delivery tag.
    let ack_payload = AckPayload {
        subscription_id: sub_id,
    }
    .encode()?;

    let ack_frame = Frame {
        msg_type: FrameType::Ack,
        correlation_id: delivery_frame.correlation_id,
        payload: ack_payload,
    };

    let ack_response = handler.handle_frame(conn_id, ack_frame).await?;
    if let FrameResponse::Frame(_) = ack_response {
        return Err("unexpected frame in response to message ACK".into());
    }

    Ok(())
}

fn make_wal_config(policy: Option<&str>) -> WalConfig {
    let mut cfg = WalConfig::default();
    if let Some(raw) = policy {
        let p = raw.trim().to_ascii_lowercase();
        if p == "always" {
            cfg.fsync_every_n = Some(1);
            cfg.fsync_interval = None;
        } else if p == "none" {
            cfg.fsync_every_n = None;
            cfg.fsync_interval = None;
        } else if let Some(rest) = p.strip_prefix("every_n:") {
            if let Ok(n) = rest.parse::<usize>() {
                cfg.fsync_every_n = Some(n);
            }
        } else if let Some(rest) = p.strip_prefix("interval_ms:") {
            if let Ok(ms) = rest.parse::<u64>() {
                cfg.fsync_interval = Some(std::time::Duration::from_millis(ms));
            }
        }
    }
    cfg
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let cli = Cli::parse();

    match load_config(cli.config.as_deref()) {
        Ok(config) => {
            if config.enable_tokio_console {
                console_subscriber::init();
            } else {
                let filter =
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
                fmt().with_env_filter(filter).init();
            }

            let wal_config = make_wal_config(config.fsync_policy.as_deref());
            let wal =
                Arc::new(WriteAheadLog::open_with_config(&config.wal_path, wal_config).await?);

            let broker = Arc::new(Broker::new_with_wal(
                BrokerConfig {
                    default_qos: QoSLevel::AtLeastOnce,
                    message_ttl: std::time::Duration::from_secs(60),
                    per_subscriber_queue_capacity: 1024,
                    max_retries: config.max_retries,
                    retry_base_delay: std::time::Duration::from_millis(config.retry_backoff_ms),
                },
                wal.clone(),
            ));

            // Replay WAL at startup to restore durable state.
            broker.replay_from_wal().await?;

            let auth_validator =
                Arc::new(StaticApiKeyValidator::from_keys(&config.allowed_api_keys));

            let handler = BrokerHandler::new(broker.clone());

            // Set up a shutdown channel but do not actually start the network
            // server yet; this will be wired in a later step.
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let server = Server::new(
                NetworkConfig {
                    bind_addr: SocketAddr::new(config.bind_addr.parse().unwrap(), config.port),
                },
                handler.clone(),
                auth_validator,
                shutdown_rx,
            );

            // Spawn a maintenance task that periodically runs broker
            // maintenance (TTL expiry, retries).
            let maintenance_broker = broker.clone();
            let mut maintenance_shutdown = shutdown_tx.subscribe();
            tokio::spawn(async move {
                let mut interval = time::interval(Duration::from_millis(200));
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            maintenance_broker.maintenance_tick(std::time::Instant::now());
                        }
                        changed = maintenance_shutdown.changed() => {
                            if changed.is_err() {
                                // Sender dropped; nothing more to do.
                            }
                            break;
                        }
                    }
                }
            });

            // Basic integration-style exercise of the handler and broker.
            run_integration_flow(handler.clone(), broker.clone()).await?;

            // Start metrics server.
            let metrics_addr =
                SocketAddr::new(config.metrics_addr.parse().unwrap(), config.metrics_port);
            let metrics_broker = broker.clone();
            let metrics_wal = wal.clone();
            tokio::spawn(async move {
                if let Err(e) = run_metrics_server(metrics_addr, metrics_broker, metrics_wal).await
                {
                    error!("metrics server error: {}", e);
                }
            });

            // Start network server.
            let server_broker = broker.clone();
            tokio::spawn(async move {
                if let Err(e) = server.start().await {
                    error!("network server error: {}", e);
                }
                // Ensure broker lives for at least as long as the server.
                let _ = server_broker;
            });

            // Wait for Ctrl+C or termination signal.
            signal::ctrl_c().await?;

            // Begin graceful shutdown sequence: stop accepting new publishes,
            // signal background tasks, wait for in-flight messages with a
            // bounded timeout, then flush the WAL.
            broker.begin_shutdown();
            let _ = shutdown_tx.send(true);

            let drain_timeout = Duration::from_secs(2);
            let start = std::time::Instant::now();
            while broker.inflight_message_count() > 0 && start.elapsed() < drain_timeout {
                time::sleep(Duration::from_millis(50)).await;
            }

            broker.flush_wal().await?;

            info!(
                "BlipMQ daemon initialized with bind_addr={}:{} metrics_addr={}:{}",
                config.bind_addr, config.port, config.metrics_addr, config.metrics_port
            );
        }
        Err(err) => {
            error!("failed to load configuration: {err}");
        }
    }

    info!("BlipMQ daemon startup complete (skeleton)");
    Ok(())
}
