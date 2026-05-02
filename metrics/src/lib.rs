use std::convert::Infallible;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Arc;

use corelib::Broker;
use hyper::header::CONTENT_TYPE;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use tracing::info;
use wal::WriteAheadLog;

/// Prometheus text exposition format content-type per the Prometheus
/// docs. Versioned so scrapers know what they're parsing.
const PROMETHEUS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

async fn handle_request(
    req: Request<Body>,
    broker: Arc<Broker>,
    wal: Arc<WriteAheadLog>,
) -> Result<Response<Body>, Infallible> {
    if req.method() != Method::GET {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::from("only GET supported"))
            .unwrap());
    }

    match req.uri().path() {
        "/metrics" => Ok(metrics_response(&broker, &wal).await),
        "/healthz" => Ok(healthz_response()),
        "/readyz" => Ok(readyz_response(&broker)),
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("not found"))
            .unwrap()),
    }
}

/// Liveness probe: 200 if the metrics server is reachable. We deliberately
/// do not consult broker state here — k8s liveness should restart the pod
/// only when the process is fundamentally broken, not when it's
/// shutting down on purpose.
fn healthz_response() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/plain")
        .body(Body::from("ok\n"))
        .unwrap()
}

/// Readiness probe: 200 if the broker has completed startup (WAL replay)
/// AND is not currently shutting down. 503 otherwise so a load balancer
/// stops routing new traffic during draining.
fn readyz_response(broker: &Broker) -> Response<Body> {
    if broker.is_ready() {
        Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, "text/plain")
            .body(Body::from("ready\n"))
            .unwrap()
    } else {
        let reason = if broker.is_shutting_down() {
            "shutting down"
        } else {
            "starting up"
        };
        Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .header(CONTENT_TYPE, "text/plain")
            .body(Body::from(format!("not ready: {reason}\n")))
            .unwrap()
    }
}

async fn metrics_response(broker: &Broker, wal: &WriteAheadLog) -> Response<Body> {
    let mut out = String::with_capacity(4096);

    // Aggregate broker counters.
    write_counter(
        &mut out,
        "blipmq_topics",
        "Number of topics with at least one subscriber currently registered.",
        broker.topic_count() as u64,
    );
    write_counter(
        &mut out,
        "blipmq_subscribers",
        "Number of registered subscriptions across all topics.",
        broker.subscriber_count() as u64,
    );
    write_counter(
        &mut out,
        "blipmq_messages_published_total",
        "Total messages accepted by the broker since process start.",
        broker.messages_published_total(),
    );
    write_counter(
        &mut out,
        "blipmq_messages_delivered_total",
        "Total deliveries dispatched to subscribers (sum of fanout across all messages).",
        broker.messages_delivered_total(),
    );
    write_counter(
        &mut out,
        "blipmq_messages_inflight",
        "Number of QoS1 messages currently in-flight awaiting ACK.",
        broker.inflight_message_count() as u64,
    );
    write_counter(
        &mut out,
        "blipmq_push_dropped_total",
        "Deliveries dropped because a push subscriber's channel was full.",
        broker.push_dropped_total(),
    );
    write_counter(
        &mut out,
        "blipmq_subscriber_expiration_heap_size",
        "Total entries in per-subscriber TTL heaps across all subscribers.",
        broker.expiration_heap_size() as u64,
    );
    write_counter(
        &mut out,
        "blipmq_subscriber_retry_heap_size",
        "Total entries in per-subscriber retry heaps across all subscribers.",
        broker.retry_heap_size() as u64,
    );

    // WAL counters.
    let (wal_appends_total, wal_bytes_total) = wal.metrics().await;
    write_counter(
        &mut out,
        "blipmq_wal_appends_total",
        "Total records written to the WAL since process start.",
        wal_appends_total,
    );
    write_counter(
        &mut out,
        "blipmq_wal_bytes_total",
        "Total bytes (record header + payload) written to the WAL since process start.",
        wal_bytes_total,
    );

    // Publish-fanout latency as a Prometheus summary. Quantiles only
    // (we use hdrhistogram internally, but a Prometheus *histogram*
    // would require pre-declared bucket boundaries; summary with
    // {quantile="..."} keeps the wire format self-describing).
    let lat = broker.publish_fanout_latency();
    let _ = writeln!(
        out,
        "# HELP blipmq_publish_fanout_seconds Time spent fanning out one publish across all subscribers."
    );
    let _ = writeln!(out, "# TYPE blipmq_publish_fanout_seconds summary");
    let _ = writeln!(
        out,
        "blipmq_publish_fanout_seconds{{quantile=\"0.5\"}} {:.9}",
        lat.p50_ns as f64 / 1e9,
    );
    let _ = writeln!(
        out,
        "blipmq_publish_fanout_seconds{{quantile=\"0.95\"}} {:.9}",
        lat.p95_ns as f64 / 1e9,
    );
    let _ = writeln!(
        out,
        "blipmq_publish_fanout_seconds{{quantile=\"0.99\"}} {:.9}",
        lat.p99_ns as f64 / 1e9,
    );
    let _ = writeln!(out, "blipmq_publish_fanout_seconds_count {}", lat.count,);
    let _ = writeln!(
        out,
        "blipmq_publish_fanout_seconds_sum {:.9}",
        lat.sum_ns as f64 / 1e9,
    );

    // Per-topic counters as labelled metrics. One scrape walks every
    // topic shard under read locks; cost is O(num_topics).
    let topic_metrics = broker.topic_metrics();
    if !topic_metrics.is_empty() {
        let _ = writeln!(
            out,
            "# HELP blipmq_topic_published_total Messages published to a topic since process start."
        );
        let _ = writeln!(out, "# TYPE blipmq_topic_published_total counter");
        for tm in &topic_metrics {
            let _ = writeln!(
                out,
                r#"blipmq_topic_published_total{{topic="{}"}} {}"#,
                escape_label(tm.topic.as_str()),
                tm.published_total,
            );
        }

        let _ = writeln!(
            out,
            "# HELP blipmq_topic_delivered_total Deliveries dispatched for a topic since process start."
        );
        let _ = writeln!(out, "# TYPE blipmq_topic_delivered_total counter");
        for tm in &topic_metrics {
            let _ = writeln!(
                out,
                r#"blipmq_topic_delivered_total{{topic="{}"}} {}"#,
                escape_label(tm.topic.as_str()),
                tm.delivered_total,
            );
        }

        let _ = writeln!(
            out,
            "# HELP blipmq_topic_subscribers Active subscribers on a topic right now."
        );
        let _ = writeln!(out, "# TYPE blipmq_topic_subscribers gauge");
        for tm in &topic_metrics {
            let _ = writeln!(
                out,
                r#"blipmq_topic_subscribers{{topic="{}"}} {}"#,
                escape_label(tm.topic.as_str()),
                tm.subscriber_count,
            );
        }
    }

    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, PROMETHEUS_CONTENT_TYPE)
        .body(Body::from(out))
        .unwrap()
}

#[inline]
fn write_counter(out: &mut String, name: &str, help: &str, value: u64) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {value}");
}

/// Escape a string for use as a Prometheus label value. Per
/// <https://prometheus.io/docs/instrumenting/exposition_formats/#text-format-details>,
/// backslash, double quote, and newline must be escaped.
fn escape_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

/// Run an HTTP server exposing broker and WAL metrics, plus k8s-style
/// `/healthz` (liveness) and `/readyz` (readiness) probes.
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
    info!(
        "metrics server listening on {} (/metrics, /healthz, /readyz)",
        addr
    );
    server.await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_escape_handles_backslash_and_quote_and_newline() {
        assert_eq!(escape_label(r#"foo"bar\baz"#), r#"foo\"bar\\baz"#);
        assert_eq!(escape_label("with\nnewline"), "with\\nnewline");
        assert_eq!(escape_label("plain"), "plain");
    }
}
