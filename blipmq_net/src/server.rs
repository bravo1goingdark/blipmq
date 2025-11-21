use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use blipmq_auth::ApiKeyValidator;
use blipmq_core::{Broker, ClientId, DeliveryTag, QoSLevel, SubscriptionId, TopicName};
use bytes::Bytes;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tracing::{debug, error, info};

use crate::connection::Connection;
use crate::error::Error;
use crate::frame::{
    AckPayload, Frame, FrameType, NackPayload, PollPayload, PublishPayload, SubscribePayload,
};

#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub bind_addr: SocketAddr,
}

#[derive(Debug)]
pub enum FrameResponse {
    None,
    Frame(Frame),
}

#[async_trait]
pub trait MessageHandler: Send + Sync + 'static {
    async fn handle_frame(&self, conn_id: u64, frame: Frame) -> Result<FrameResponse, Error>;
}

pub struct Server<H>
where
    H: MessageHandler + Clone,
{
    config: NetworkConfig,
    handler: H,
    shutdown: watch::Receiver<bool>,
    auth_validator: Arc<dyn ApiKeyValidator>,
}

impl<H> Server<H>
where
    H: MessageHandler + Clone,
{
    pub fn new(
        config: NetworkConfig,
        handler: H,
        auth_validator: Arc<dyn ApiKeyValidator>,
        shutdown: watch::Receiver<bool>,
    ) -> Self {
        Self {
            config,
            handler,
            shutdown,
            auth_validator,
        }
    }

    /// Run the accept loop until a shutdown signal is received.
    pub async fn start(&self) -> Result<(), Error> {
        let span = tracing::info_span!("net_server", bind_addr = %self.config.bind_addr);
        let _guard = span.enter();
        let listener = TcpListener::bind(self.config.bind_addr).await?;
        let local_addr = listener.local_addr()?;
        info!("blipmq_net listening on {}", local_addr);

        let mut next_conn_id: u64 = 1;
        let mut shutdown_rx = self.shutdown.clone();

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, addr)) => {
                            let conn_id = next_conn_id;
                            next_conn_id = next_conn_id.wrapping_add(1);

                            let handler = self.handler.clone();
                            let conn_shutdown = shutdown_rx.clone();
                            let auth = self.auth_validator.clone();

                            debug!("accepted connection {} from {}", conn_id, addr);

                            let connection = Connection::new(conn_id, stream, handler, auth, conn_shutdown);
                            tokio::spawn(async move {
                                connection.run().await;
                            });
                        }
                        Err(err) => {
                            error!("accept error: {}", err);
                        }
                    }
                }
                result = shutdown_rx.changed() => {
                    match result {
                        Ok(_) => {
                            info!("shutdown signal received; stopping accept loop");
                            break;
                        }
                        Err(_) => {
                            info!("shutdown sender dropped; stopping accept loop");
                            break;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[derive(Clone)]
pub struct BrokerHandler {
    broker: Arc<Broker>,
}

impl BrokerHandler {
    pub fn new(broker: Arc<Broker>) -> Self {
        Self { broker }
    }

    fn qos_from_u8(&self, v: u8) -> Option<QoSLevel> {
        match v {
            0 => Some(QoSLevel::AtMostOnce),
            1 => Some(QoSLevel::AtLeastOnce),
            _ => None,
        }
    }

    fn make_nack(&self, correlation_id: u64, code: u16, message: &str) -> Result<Frame, Error> {
        let payload = NackPayload {
            code,
            message: message.to_string(),
        }
        .encode()?;
        Ok(Frame {
            msg_type: FrameType::Nack,
            correlation_id,
            payload,
        })
    }
}

#[async_trait]
impl MessageHandler for BrokerHandler {
    async fn handle_frame(&self, conn_id: u64, frame: Frame) -> Result<FrameResponse, Error> {
        match frame.msg_type {
            FrameType::Publish => self.handle_publish(conn_id, frame).await,
            FrameType::Subscribe => self.handle_subscribe(conn_id, frame).await,
            FrameType::Ack => self.handle_ack(conn_id, frame).await,
            FrameType::Ping => {
                let pong = Frame {
                    msg_type: FrameType::Pong,
                    correlation_id: frame.correlation_id,
                    payload: Bytes::new(),
                };
                Ok(FrameResponse::Frame(pong))
            }
            FrameType::Poll => self.handle_poll(conn_id, frame).await,
            FrameType::Pong | FrameType::Nack | FrameType::Hello | FrameType::Auth => {
                Ok(FrameResponse::None)
            }
        }
    }
}

impl BrokerHandler {
    async fn handle_publish(&self, _conn_id: u64, frame: Frame) -> Result<FrameResponse, Error> {
        let decoded = PublishPayload::decode(&frame.payload);
        let payload = match decoded {
            Ok(p) => p,
            Err(_) => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid PUBLISH payload")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        if payload.topic.is_empty() {
            let nack = self.make_nack(frame.correlation_id, 400, "empty topic")?;
            return Ok(FrameResponse::Frame(nack));
        }

        let qos = match self.qos_from_u8(payload.qos) {
            Some(q) => q,
            None => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid QoS value")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        let topic = TopicName::new(payload.topic);
        if qos == QoSLevel::AtLeastOnce && self.broker.has_wal() {
            // Use the WAL-backed path when configured for QoS1 messages.
            if let Err(e) = self
                .broker
                .publish_durable(&topic, payload.message, qos)
                .await
            {
                let nack = self.make_nack(
                    frame.correlation_id,
                    500,
                    &format!("durable publish failed: {e}"),
                )?;
                return Ok(FrameResponse::Frame(nack));
            }
        } else {
            self.broker.publish(&topic, payload.message, qos);
        }

        Ok(FrameResponse::None)
    }

    async fn handle_subscribe(&self, conn_id: u64, frame: Frame) -> Result<FrameResponse, Error> {
        let decoded = SubscribePayload::decode(&frame.payload);
        let payload = match decoded {
            Ok(p) => p,
            Err(_) => {
                let nack =
                    self.make_nack(frame.correlation_id, 400, "invalid SUBSCRIBE payload")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        if payload.topic.is_empty() {
            let nack = self.make_nack(frame.correlation_id, 400, "empty topic")?;
            return Ok(FrameResponse::Frame(nack));
        }

        let qos = match self.qos_from_u8(payload.qos) {
            Some(q) => q,
            None => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid QoS value")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        let client_id = ClientId::new(format!("conn-{conn_id}"));
        let topic = TopicName::new(payload.topic);

        let sub_id = self.broker.subscribe(client_id, topic, qos);

        let ack_payload = AckPayload {
            subscription_id: sub_id.value(),
        }
        .encode()?;

        let ack_frame = Frame {
            msg_type: FrameType::Ack,
            correlation_id: frame.correlation_id,
            payload: ack_payload,
        };

        Ok(FrameResponse::Frame(ack_frame))
    }

    async fn handle_ack(&self, _conn_id: u64, frame: Frame) -> Result<FrameResponse, Error> {
        let decoded = AckPayload::decode(&frame.payload);
        let payload = match decoded {
            Ok(p) => p,
            Err(_) => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid ACK payload")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        if payload.subscription_id == 0 {
            let nack = self.make_nack(
                frame.correlation_id,
                400,
                "subscription_id must be non-zero",
            )?;
            return Ok(FrameResponse::Frame(nack));
        }

        let sub_id = SubscriptionId::from_raw(payload.subscription_id);
        let tag = DeliveryTag::from_raw(frame.correlation_id);

        if self.broker.ack(sub_id, tag) {
            Ok(FrameResponse::None)
        } else {
            let nack = self.make_nack(
                frame.correlation_id,
                404,
                "unknown subscription or delivery tag",
            )?;
            Ok(FrameResponse::Frame(nack))
        }
    }

    async fn handle_poll(&self, _conn_id: u64, frame: Frame) -> Result<FrameResponse, Error> {
        let decoded = PollPayload::decode(&frame.payload);
        let payload = match decoded {
            Ok(p) => p,
            Err(_) => {
                let nack = self.make_nack(frame.correlation_id, 400, "invalid POLL payload")?;
                return Ok(FrameResponse::Frame(nack));
            }
        };

        if payload.subscription_id == 0 {
            let nack = self.make_nack(
                frame.correlation_id,
                400,
                "subscription_id must be non-zero",
            )?;
            return Ok(FrameResponse::Frame(nack));
        }

        let sub_id = SubscriptionId::from_raw(payload.subscription_id);
        let polled = match self.broker.poll(sub_id) {
            Some(m) => m,
            None => return Ok(FrameResponse::None),
        };

        let qos_byte = match polled.qos {
            QoSLevel::AtMostOnce => 0,
            QoSLevel::AtLeastOnce => 1,
        };

        let payload_bytes = PublishPayload {
            topic: polled.topic.as_str().to_string(),
            qos: qos_byte,
            message: polled.payload,
        }
        .encode()?;

        let correlation_id = match polled.delivery_tag {
            Some(tag) => tag.value(),
            None => frame.correlation_id,
        };

        let publish_frame = Frame {
            msg_type: FrameType::Publish,
            correlation_id,
            payload: payload_bytes,
        };

        Ok(FrameResponse::Frame(publish_frame))
    }
}
