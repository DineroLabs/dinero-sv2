//! End-to-end tests for the PUBLIC, unauthenticated stats listener over a
//! real socket, plus the isolation contract between it and the ops
//! listener: `/api/*` is served only on the public bind, and no ops route
//! (`/status`, `/payout-address`, `/fee-bps`, `/ban`, `/withdraw`) is
//! reachable on it — ever, regardless of method or token.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use dinero_sv2_pool::accounting::WindowEntry;
use dinero_sv2_pool::bans::BanList;
use dinero_sv2_pool::ops::{self, OpsStatus};
use dinero_sv2_pool::stats::{self, FoundBlock, Sample};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// `din1pfxwz…ggxy` decoded by an independent bech32m implementation.
const KNOWN_ADDR: &str = "din1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggxy";
const KNOWN_SCRIPT_HEX: &str =
    "5120499c2aee9ac29d750af0ad6a752ab3c549b8c1862b3bc31a0091d9715a676bd8";
/// Valid address, never submitted a share.
const STRANGER_ADDR: &str = "din1p4w46h2at4w46h2at4w46h2at4w46h2at4w46h2at4w46h2at4w4sy4ndpg";

const NOW: u64 = 1_700_000_000;

fn sample() -> Sample {
    let script = hex::decode(KNOWN_SCRIPT_HEX).unwrap();
    let other = {
        let mut s = script.clone();
        s[10] ^= 0xff;
        s
    };
    // 1 share of weight 1000 every 10s for the last 10 minutes from the
    // known miner, plus one share 2h ago from another miner (outside the
    // hashrate window but inside the PPLNS window).
    let mut window: Vec<WindowEntry> = (0..60)
        .map(|i| WindowEntry {
            payout_script: script.clone(),
            weight: 1_000,
            unix_ts: NOW - 600 + i * 10,
        })
        .collect();
    window.insert(
        0,
        WindowEntry {
            payout_script: other,
            weight: 1_000,
            unix_ts: NOW - 7_200,
        },
    );
    Sample {
        now_unix: NOW,
        window,
        blocks: (0..5)
            .map(|i| FoundBlock {
                height: 1_000 + i,
                hash: format!("{:064x}", i),
                time: NOW - 100_000 + i * 20_000,
                reward_una: 50_000_000,
                status: "accepted".into(),
                outputs: vec![(KNOWN_SCRIPT_HEX.into(), 49_000_000)],
            })
            .collect(),
        blocks_found_total: 12,
        fee_bps: 200,
        min_payout_una: 10_000,
        stratum: "pool.example:4444".into(),
        network_height: 123_456,
        network_nbits: 0x1d00ffff,
        next_reward_una: 50_000_000,
        connected_workers: 4,
    }
}

fn config(max_requests: u32) -> stats::Config {
    stats::Config {
        rate_limit: stats::RateLimit {
            max_requests,
            window_secs: 60,
        },
        cache_ttl: Duration::from_millis(10),
        ..stats::Config::default()
    }
}

async fn start_public(max_requests: u32) -> String {
    start_with(config(max_requests)).await
}

async fn start_with(cfg: stats::Config) -> String {
    let listener = stats::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        let _ = stats::serve(listener, Arc::new(sample), cfg).await;
    });
    addr
}

fn canned_ops() -> OpsStatus {
    OpsStatus {
        schema_version: 2,
        schema_min_compatible: Some(2),
        generated_at_unix: NOW,
        payout_address: KNOWN_ADDR.into(),
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
        miners: vec![],
        stratum_bind: "127.0.0.1:4444".into(),
        daemon_connected: true,
        daemon_endpoint: "http://127.0.0.1:20998".into(),
        daemon_blocks: 100,
        daemon_headers: 100,
        template_height: 101,
        template_id: 9,
        template_prev_hash: "00aa".into(),
        last_template_at_unix: NOW,
        last_share: None,
        last_block: None,
        rejection_reasons: BTreeMap::new(),
        bans: Vec::new(),
    }
}

async fn start_ops() -> String {
    let listener = ops::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let apply = Arc::new(|a: String| async move { Ok(a) });
    let apply_fee = Arc::new(|fee: u32| async move { Ok(fee) });
    tokio::spawn(async move {
        let _ = ops::serve(
            listener,
            "tok-abc".to_string(),
            ops::Policy::default(),
            Arc::new(canned_ops),
            apply,
            apply_fee,
            Arc::new(BanList::default()),
        )
        .await;
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

async fn get(addr: &str, path: &str) -> String {
    raw(addr, &format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n")).await
}

fn status_line(resp: &str) -> &str {
    resp.lines().next().unwrap_or("")
}

fn body_json(resp: &str) -> serde_json::Value {
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    serde_json::from_str(body).unwrap_or_else(|e| panic!("not json ({e}): {resp}"))
}

fn has_header(resp: &str, name: &str, value: &str) -> bool {
    let head = resp.split("\r\n\r\n").next().unwrap_or("");
    head.lines().any(|l| {
        l.split_once(':')
            .map(|(n, v)| n.trim().eq_ignore_ascii_case(name) && v.trim() == value)
            .unwrap_or(false)
    })
}

// ---- (a) /api/stats shape ----

#[tokio::test]
async fn api_stats_has_the_documented_shape_and_values() {
    let addr = start_public(100).await;
    let resp = get(&addr, "/api/stats").await;
    assert_eq!(status_line(&resp), "HTTP/1.1 200 OK", "{resp}");
    assert!(resp.contains("application/json"), "{resp}");
    let v = body_json(&resp);

    // 60 shares × weight 1000 over a 600s window = 100 H/s.
    assert_eq!(v["pool_hashrate_hs"], 100);
    // Only the known miner has a share in the last 10 minutes.
    assert_eq!(v["miners"], 1);
    assert_eq!(v["workers"], 4);
    // Block times: NOW-100000, -80000, -60000, -40000, -20000 → 4 within 86400s.
    assert_eq!(v["blocks_found_24h"], 4);
    assert_eq!(v["blocks_found_total"], 12);
    assert_eq!(v["last_block_height"], 1_004);
    assert_eq!(v["last_block_hash"], format!("{:064x}", 4));
    assert_eq!(v["last_block_time"], NOW - 20_000);
    assert_eq!(v["fee_bps"], 200);
    assert_eq!(v["payout_scheme"], "PPLNS");
    assert_eq!(v["min_payout_una"], 10_000);
    assert_eq!(v["stratum"], "pool.example:4444");
    assert_eq!(v["network_height"], 123_456);
    assert_eq!(v["network_difficulty"], 1.0);
    assert_eq!(v["updated_at"], NOW);

    // Nothing an operator would not want public.
    let text = resp.to_lowercase();
    for forbidden in ["payout_address", "token", "withdraw", "daemon_endpoint"] {
        assert!(!text.contains(forbidden), "leaked {forbidden}: {resp}");
    }
}

// ---- (b) /api/blocks limit clamping ----

#[tokio::test]
async fn api_blocks_returns_newest_first_and_clamps_limit() {
    let addr = start_public(100).await;

    let v = body_json(&get(&addr, "/api/blocks").await);
    let all = v.as_array().expect("array");
    assert_eq!(all.len(), 5);
    assert_eq!(all[0]["height"], 1_004, "newest first");
    assert_eq!(all[4]["height"], 1_000);
    for b in all {
        for key in ["height", "hash", "time", "reward_una", "status"] {
            assert!(b.get(key).is_some(), "missing {key}: {b}");
        }
        assert!(b.get("outputs").is_none(), "per-block outputs are internal");
    }

    let v = body_json(&get(&addr, "/api/blocks?limit=2").await);
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert_eq!(v[0]["height"], 1_004);

    // Zero, negative, junk and absurd values clamp rather than 400.
    for q in [
        "limit=0",
        "limit=-1",
        "limit=abc",
        "limit=99999999999999999999",
    ] {
        let resp = get(&addr, &format!("/api/blocks?{q}")).await;
        assert_eq!(status_line(&resp), "HTTP/1.1 200 OK", "{q}: {resp}");
        let n = body_json(&resp).as_array().unwrap().len();
        assert!((1..=5).contains(&n), "{q} gave {n}");
    }
}

// ---- (c) /api/miner/<addr> ----

#[tokio::test]
async fn api_miner_reports_a_known_address() {
    let addr = start_public(100).await;
    let resp = get(&addr, &format!("/api/miner/{KNOWN_ADDR}")).await;
    assert_eq!(status_line(&resp), "HTTP/1.1 200 OK", "{resp}");
    let v = body_json(&resp);
    assert_eq!(v["address"], KNOWN_ADDR);
    assert_eq!(v["hashrate_hs"], 100);
    assert_eq!(v["shares_1h"], 60);
    // 60_000 of 61_000 total window weight.
    assert_eq!(v["window_bps"], 9_836);
    // Estimated next-block payout: 50_000_000 × 0.98 × 9836/10000.
    assert_eq!(v["est_next_block_una"], 48_196_400);
    assert_eq!(v["paid_una"], 5 * 49_000_000u64);
    assert_eq!(v["last_share_time"], NOW - 10);
}

#[tokio::test]
async fn api_miner_is_404_for_an_unknown_address_and_400_for_junk() {
    let addr = start_public(100).await;
    let resp = get(&addr, &format!("/api/miner/{STRANGER_ADDR}")).await;
    assert_eq!(status_line(&resp), "HTTP/1.1 404 Not Found", "{resp}");
    // No hint of which addresses exist.
    let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
    assert!(!body.contains("din1p"), "{resp}");

    for junk in [
        "",
        "not-an-address",
        "din1qqqq",
        "bc1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggxy",
    ] {
        let resp = get(&addr, &format!("/api/miner/{junk}")).await;
        let line = status_line(&resp);
        assert!(
            line == "HTTP/1.1 400 Bad Request" || line == "HTTP/1.1 404 Not Found",
            "{junk:?}: {resp}"
        );
    }
    // There is no listing.
    let resp = get(&addr, "/api/miner").await;
    assert_eq!(status_line(&resp), "HTTP/1.1 404 Not Found", "{resp}");
    let resp = get(&addr, "/api/miners").await;
    assert_eq!(status_line(&resp), "HTTP/1.1 404 Not Found", "{resp}");
}

// ---- (d) isolation ----

#[tokio::test]
async fn public_listener_never_serves_an_ops_route() {
    let addr = start_public(1_000).await;
    for path in [
        ops::STATUS_PATH,
        ops::PAYOUT_PATH,
        ops::FEE_PATH,
        ops::BAN_PATH,
        ops::UNBAN_PATH,
        "/withdraw",
        "/config",
    ] {
        for method in ["GET", "POST", "HEAD"] {
            // Even presenting a (real or guessed) ops token changes nothing.
            let resp = raw(
                &addr,
                &format!(
                    "{method} {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok-abc\r\n\
                     Content-Length: 2\r\n\r\n{{}}"
                ),
            )
            .await;
            assert_eq!(
                status_line(&resp),
                "HTTP/1.1 404 Not Found",
                "{method} {path} on the PUBLIC listener: {resp}"
            );
            let body = resp.split("\r\n\r\n").nth(1).unwrap_or("");
            assert!(
                !body.contains("payout_address") && !body.contains("\"ok\""),
                "{method} {path} leaked: {resp}"
            );
        }
    }
}

#[tokio::test]
async fn ops_listener_never_serves_a_public_api_route() {
    let addr = start_ops().await;
    for path in [
        "/api/stats",
        "/api/blocks",
        &format!("/api/miner/{KNOWN_ADDR}"),
        "/",
    ] {
        // With the real token: route does not exist there.
        let resp = raw(
            &addr,
            &format!("GET {path} HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer tok-abc\r\n\r\n"),
        )
        .await;
        assert_eq!(
            status_line(&resp),
            "HTTP/1.1 404 Not Found",
            "{path} on the OPS listener: {resp}"
        );
        // Without a token: still no body, and no hint the route is missing.
        let resp = get(&addr, path).await;
        assert_eq!(
            status_line(&resp),
            "HTTP/1.1 401 Unauthorized",
            "{path}: {resp}"
        );
        assert!(!resp.contains("pool_hashrate_hs"), "{path}: {resp}");
    }
}

// ---- (e) rate limit ----

#[tokio::test]
async fn a_client_over_the_per_ip_limit_gets_429() {
    let addr = start_public(3).await;
    for i in 0..3 {
        let resp = get(&addr, "/api/stats").await;
        assert_eq!(status_line(&resp), "HTTP/1.1 200 OK", "request {i}: {resp}");
    }
    let resp = get(&addr, "/api/stats").await;
    assert_eq!(
        status_line(&resp),
        "HTTP/1.1 429 Too Many Requests",
        "{resp}"
    );
    assert!(has_header(&resp, "Retry-After", "60"), "{resp}");
}

// ---- (f) CORS ----

#[tokio::test]
async fn cors_header_is_present_on_get_and_absent_on_other_methods() {
    let addr = start_public(100).await;
    for path in [
        "/api/stats",
        "/api/blocks",
        &format!("/api/miner/{KNOWN_ADDR}"),
    ] {
        let resp = get(&addr, path).await;
        assert!(
            has_header(&resp, "Access-Control-Allow-Origin", "*"),
            "GET {path} lacks CORS: {resp}"
        );
    }
    // A 404 on a GET is still a cross-origin-readable answer.
    let resp = get(&addr, &format!("/api/miner/{STRANGER_ADDR}")).await;
    assert!(
        has_header(&resp, "Access-Control-Allow-Origin", "*"),
        "{resp}"
    );

    for method in ["POST", "PUT", "DELETE", "PATCH"] {
        let resp = raw(
            &addr,
            &format!("{method} /api/stats HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n"),
        )
        .await;
        assert_eq!(
            status_line(&resp),
            "HTTP/1.1 405 Method Not Allowed",
            "{method}: {resp}"
        );
        assert!(
            !resp.to_lowercase().contains("access-control-allow-origin"),
            "{method} carries CORS: {resp}"
        );
    }
}

// ---- index page ----

#[tokio::test]
async fn index_serves_html_that_fetches_the_stats_api() {
    let addr = start_public(100).await;
    let resp = get(&addr, "/").await;
    assert_eq!(status_line(&resp), "HTTP/1.1 200 OK", "{resp}");
    assert!(resp.contains("text/html"), "{resp}");
    assert!(resp.contains("/api/stats"), "{resp}");
    assert!(resp.contains("install.sh"), "{resp}");
    assert!(resp.contains("install.ps1"), "{resp}");
    // Polls no faster than every 30s: 60 req/min per client must leave
    // room for a few open tabs behind one NAT.
    assert!(resp.contains("setInterval(load, 30000)"), "{resp}");
    // HEAD gets the headers and no body.
    let resp = raw(&addr, "HEAD / HTTP/1.1\r\nHost: x\r\n\r\n").await;
    assert_eq!(status_line(&resp), "HTTP/1.1 200 OK", "{resp}");
    assert_eq!(resp.split("\r\n\r\n").nth(1).unwrap_or(""), "");
}

// ---- cache ----

#[tokio::test]
async fn the_sample_is_cached_for_the_configured_ttl() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let listener = stats::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let c = calls.clone();
    let source = Arc::new(move || {
        c.fetch_add(1, Ordering::SeqCst);
        sample()
    });
    let cfg = stats::Config {
        rate_limit: stats::RateLimit {
            max_requests: 1_000,
            window_secs: 60,
        },
        cache_ttl: Duration::from_secs(10),
        ..stats::Config::default()
    };
    tokio::spawn(async move {
        let _ = stats::serve(listener, source, cfg).await;
    });
    for _ in 0..5 {
        get(&addr, "/api/stats").await;
        get(&addr, "/api/blocks").await;
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "sample rebuilt inside the TTL"
    );
}

// ---- rate limit keying behind a reverse proxy ----

// Behind nginx every peer is 127.0.0.1. The limiter must key on the
// forwarded client, or the whole internet shares one bucket.
#[tokio::test]
async fn loopback_peers_are_keyed_by_the_forwarded_client_address() {
    let addr = start_public(1).await;
    let req = |hdr: &str| format!("GET /api/stats HTTP/1.1\r\nHost: x\r\n{hdr}\r\n\r\n");
    let a = raw(&addr, &req("X-Forwarded-For: 203.0.113.1, 10.0.0.1")).await;
    assert_eq!(status_line(&a), "HTTP/1.1 200 OK", "{a}");
    let b = raw(&addr, &req("X-Forwarded-For: 203.0.113.2")).await;
    assert_eq!(
        status_line(&b),
        "HTTP/1.1 200 OK",
        "second client shares a bucket: {b}"
    );
    let c = raw(&addr, &req("X-Forwarded-For: 203.0.113.1")).await;
    assert_eq!(status_line(&c), "HTTP/1.1 429 Too Many Requests", "{c}");
    let d = raw(&addr, &req("X-Real-IP: 203.0.113.3")).await;
    assert_eq!(status_line(&d), "HTTP/1.1 200 OK", "{d}");
    // Case-insensitive header names, as everywhere else.
    let e = raw(&addr, &req("x-forwarded-for: 203.0.113.2")).await;
    assert_eq!(status_line(&e), "HTTP/1.1 429 Too Many Requests", "{e}");
}

// ---- connection cap ----

#[tokio::test]
async fn connections_beyond_the_cap_are_closed_without_a_response() {
    let addr = start_with(stats::Config {
        max_connections: 2,
        ..config(1_000)
    })
    .await;
    let held1 = TcpStream::connect(&addr).await.unwrap();
    let held2 = TcpStream::connect(&addr).await.unwrap();
    // Let the acceptor take both permits.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut third = TcpStream::connect(&addr).await.unwrap();
    let _ = third
        .write_all(b"GET /api/stats HTTP/1.1\r\nHost: x\r\n\r\n")
        .await;
    let mut out = String::new();
    match tokio::time::timeout(Duration::from_secs(2), third.read_to_string(&mut out)).await {
        Ok(_) => {}
        Err(_) => panic!("third connection was neither served nor closed"),
    }
    assert!(out.is_empty(), "over-cap connection got a response: {out}");

    // Permits return when the held connections end.
    drop(held1);
    drop(held2);
    let mut served = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if status_line(&get(&addr, "/api/stats").await) == "HTTP/1.1 200 OK" {
            served = true;
            break;
        }
    }
    assert!(served, "permits were not released");
}

// ---- slowloris ----

#[tokio::test]
async fn a_stalled_connection_is_closed_at_the_deadline() {
    let addr = start_with(stats::Config {
        connection_timeout: Duration::from_millis(300),
        ..config(1_000)
    })
    .await;
    let started = std::time::Instant::now();
    let mut sock = TcpStream::connect(&addr).await.unwrap();
    sock.write_all(b"G").await.unwrap();
    let mut out = Vec::new();
    match tokio::time::timeout(Duration::from_secs(3), sock.read_to_end(&mut out)).await {
        Ok(_) => {}
        Err(_) => panic!("stalled connection was held open past the deadline"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
    assert!(
        out.is_empty(),
        "a half-request got a response: {:?}",
        String::from_utf8_lossy(&out)
    );
}
