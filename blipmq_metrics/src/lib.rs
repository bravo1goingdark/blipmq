use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use blipmq_core::Broker;
use blipmq_wal::WriteAheadLog;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Request, Response, Server};
use tracing::info;

async fn handle_request(
    req: Request<Body>,
    broker: Arc<Broker>,
    wal: Arc<WriteAheadLog>,
) -> Result<Response<Body>, Infallible> {
    if req.uri().path() != "/metrics" {
        return Ok(Response::builder()
            .status(404)
            .body(Body::from("not found"))
            .unwrap());
    }

    let topics = broker.topic_count();
    let subscribers = broker.subscriber_count();
    let published = broker.messages_published_total();
    let delivered = broker.messages_delivered_total();
    let inflight = broker.inflight_message_count();

    let (wal_appends_total, wal_bytes_total) = wal.metrics().await;

    let body = format!(
        "topics {topics}\nsubscribers {subscribers}\nmessages_published_total {published}\nmessages_delivered_total {delivered}\nmessages_inflight {inflight}\nwal_appends_total {wal_appends_total}\nwal_bytes_total {wal_bytes_total}\n"
    );

    Ok(Response::new(Body::from(body)))
}

/// Run an HTTP server exposing broker and WAL metrics.
pub async fn run_metrics_server(
    addr: SocketAddr,
    broker: Arc<Broker>,
    wal: Arc<WriteAheadLog>,
) -> Result<(), hyper::Error> {
    let make_svc = make_service_fn(move |_conn| {
        let broker = broker.clone();
        let wal = wal.clone();
        async move {
            Ok::<_, Infallible>(service_fn(move |req| {
                handle_request(req, broker.clone(), wal.clone())
            }))
        }
    });

    let server = Server::bind(&addr).serve(make_svc);
    info!("metrics server listening on {}", addr);
    server.await
}
