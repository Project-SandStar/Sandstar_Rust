//! Integration tests for the `GET /metrics` Prometheus exposition endpoint.
//!
//! Unlike the unit tests in `metrics::tests` (which exercise the text
//! formatter), these tests prove the endpoint is reachable through the
//! axum router on a real ephemeral TCP listener, returns HTTP 200 with
//! the right `Content-Type`, and the body contains the expected metric
//! names — including the driver-framework counters wired in the actor.

mod common;

use std::time::Duration;

use common::TestServer;

/// Minimal HTTP GET that returns (status, content-type, body). Kept
/// local so we don't pull `reqwest` into this file's dependency graph
/// just for one call.
async fn http_get(host_port: &str, path: &str) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(host_port)
        .await
        .expect("connect");
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).await.expect("write");
    stream.flush().await.expect("flush");

    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf)).await;
    let resp = String::from_utf8_lossy(&buf).to_string();

    let status = resp
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    // Extract Content-Type from headers (case-insensitive).
    let content_type = resp
        .lines()
        .take_while(|l| !l.is_empty())
        .find(|l| l.to_ascii_lowercase().starts_with("content-type:"))
        .map(|l| l.splitn(2, ':').nth(1).unwrap_or("").trim().to_string())
        .unwrap_or_default();

    let body = resp
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (status, content_type, body)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prometheus_endpoint_returns_text_plain_200() {
    let server = TestServer::start().await;
    // `TestServer::url(path)` returns `http://127.0.0.1:PORT/path`; the
    // `host_port` HTTP helper wants just the host:port.
    let host_port = server
        .url("")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();

    let (status, content_type, _body) = http_get(&host_port, "/metrics").await;
    assert_eq!(status, 200, "/metrics should return 200");
    assert!(
        content_type.starts_with("text/plain"),
        "/metrics Content-Type should be text/plain; got {content_type:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prometheus_body_contains_expected_sections() {
    let server = TestServer::start().await;
    let host_port = server
        .url("")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();

    let (_, _, body) = http_get(&host_port, "/metrics").await;

    // Build info gauge — labels must include a version.
    assert!(
        body.contains("sandstar_build_info{"),
        "expected sandstar_build_info line, got body starting:\n{}",
        &body.chars().take(400).collect::<String>()
    );

    // A counter we wired into the driver actor.
    assert!(
        body.contains("# TYPE sandstar_driver_cov_events_total counter"),
        "driver CovEvent counter missing"
    );
    assert!(
        body.contains("# TYPE sandstar_driver_sync_total counter"),
        "driver sync counter missing"
    );
    assert!(
        body.contains("# TYPE sandstar_driver_sync_errors_total counter"),
        "driver sync_errors counter missing"
    );

    // A gauge we expect to see.
    assert!(
        body.contains("# TYPE sandstar_ws_active gauge"),
        "ws_active gauge missing"
    );
    assert!(
        body.contains("# TYPE sandstar_watch_active gauge"),
        "watch_active gauge missing"
    );
}

/// Verify each non-comment line in the body is well-formed per the
/// Prometheus exposition spec: `<metric_name>[{labels}] <value>`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prometheus_body_is_well_formed() {
    let server = TestServer::start().await;
    let host_port = server
        .url("")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let (_, _, body) = http_get(&host_port, "/metrics").await;

    for line in body.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Metric line: must contain a space separating name+labels from value.
        assert!(
            line.contains(' '),
            "non-comment line missing value separator: {line:?}"
        );
        // Name must start with ASCII alpha or underscore.
        let first = line.chars().next().unwrap();
        assert!(
            first.is_ascii_alphabetic() || first == '_',
            "metric name starts with invalid char: {line:?}"
        );
    }
}
