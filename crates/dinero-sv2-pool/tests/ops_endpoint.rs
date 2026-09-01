//! End-to-end tests for the operator status endpoint over a real
//! socket. The unit tests in `ops.rs` cover the routing/auth decision;
//! these cover the parts only a live connection exercises — head
//! reading, status lines, and that an unauthorized caller never gets
//! a body.

use std::sync::Arc;

use dinero_sv2_pool::ops::{self, MinerStatus, OpsStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn canned() -> OpsStatus {
    OpsStatus {
        pool_version: "test".into(),
        uptime_secs: 42,
        fee_bps: 1000,
        connected_miners: 3,
        window_entries: 1500,
        window_span_secs: 3600,
        template_heartbeat_age_secs: 1,
        template_phase: "sleeping".into(),
        accepted_shares_total: 900,
        rejected_shares_total: 4,
        blocks_found_total: 7,
        miners: vec![MinerStatus {
            payout_script_hex: "5120aa".into(),
            bps: 5000,
            window_weight: "12345".into(),
        }],
    }
}

async fn start() -> String {
    let listener = ops::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let snapshot = Arc::new(canned);
    tokio::spawn(async move {
        let _ = ops::serve(listener, "tok-abc".to_string(), snapshot).await;
    });
    addr
}

async fn raw(addr: &str, request: &str) -> String {
    let mut sock = TcpStream::connect(addr).await.unwrap();
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut out = String::new();
    sock.read_to_string(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn authorized_status_returns_json() {
    let addr = start().await;
    let resp = raw(
        &addr,
        "GET /status HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok-abc\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
    assert!(resp.contains("application/json"));
    let body = resp.split("\r\n\r\n").nth(1).unwrap();
    let parsed: OpsStatus = serde_json::from_str(body).expect("valid OpsStatus json");
    assert_eq!(parsed, canned());
    assert_eq!(parsed.fee_bps, 1000);
    assert_eq!(parsed.connected_miners, 3);
}

#[tokio::test]
async fn wrong_token_is_401_and_leaks_no_data() {
    let addr = start().await;
    let resp = raw(
        &addr,
        "GET /status HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 401 Unauthorized"), "got: {resp}");
    // Nothing about the pool may appear in a refused response.
    for leak in ["fee_bps", "connected_miners", "5120aa", "window_entries"] {
        assert!(!resp.contains(leak), "refused response leaked {leak}: {resp}");
    }
}

#[tokio::test]
async fn missing_authorization_is_401() {
    let addr = start().await;
    let resp = raw(&addr, "GET /status HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert!(resp.starts_with("HTTP/1.1 401 Unauthorized"), "got: {resp}");
}

#[tokio::test]
async fn writes_are_refused_over_the_wire() {
    let addr = start().await;
    let resp = raw(
        &addr,
        "POST /status HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok-abc\r\nContent-Length: 0\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 405 Method Not Allowed"), "got: {resp}");
}

#[tokio::test]
async fn unknown_route_is_404_when_authorized() {
    let addr = start().await;
    let resp = raw(
        &addr,
        "GET /admin HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok-abc\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 404 Not Found"), "got: {resp}");
}

#[tokio::test]
async fn head_returns_headers_without_a_body() {
    let addr = start().await;
    let resp = raw(
        &addr,
        "HEAD /status HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok-abc\r\n\r\n",
    )
    .await;
    assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    assert!(body.is_empty(), "HEAD must not send a body, got: {body}");
}

#[tokio::test]
async fn an_oversized_head_is_refused_not_buffered() {
    // The server caps the head at MAX_HEAD_BYTES and stops reading. It
    // then tries to send 400, but it has already stopped draining, so
    // the client may be reset mid-write instead. BOTH are correct
    // refusals and which one you observe is a timing race — the
    // properties under test are that the pool does not buffer the
    // oversize head, and that nothing about the pool comes back.
    let addr = start().await;
    let junk = "x".repeat(ops::MAX_HEAD_BYTES + 1024);
    let request =
        format!("GET /status HTTP/1.1\r\nX-Junk: {junk}\r\nAuthorization: Bearer tok-abc\r\n\r\n");

    let mut sock = TcpStream::connect(&addr).await.unwrap();
    let mut resp = String::new();
    if sock.write_all(request.as_bytes()).await.is_ok() {
        // Reset here is equally acceptable; treat it as an empty reply.
        let _ = sock.read_to_string(&mut resp).await;
    }
    assert!(
        resp.is_empty() || resp.starts_with("HTTP/1.1 400 Bad Request"),
        "oversize head must be refused, got: {resp}"
    );
    for leak in ["fee_bps", "connected_miners", "5120aa"] {
        assert!(!resp.contains(leak), "oversize refusal leaked {leak}");
    }
}

#[tokio::test]
async fn serve_refuses_to_start_without_a_token() {
    let listener = ops::bind("127.0.0.1:0").await.unwrap();
    let err = ops::serve(listener, String::new(), Arc::new(canned))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("without a token"), "got: {err}");
}
