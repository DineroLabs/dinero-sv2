//! Read-only operator status endpoint.
//!
//! Exists so a pool operator can see what their pool is doing without
//! shelling into the box — and so `dinero-qt` can show it.
//!
//! Deliberate constraints:
//!
//!   * **Read-only.** No route mutates anything. The endpoint cannot
//!     move funds, change the fee, or touch the PPLNS window.
//!   * **Loopback by default.** Binding to `127.0.0.1` means no TLS
//!     stack inside a binary that handles money. Remote access is a
//!     reverse proxy's job (nginx/caddy) or an SSH tunnel; both are
//!     documented rather than reimplemented here.
//!   * **Bearer token, constant-time compared.** The token is read from
//!     a file so it never appears in `ps` output or a systemd unit.
//!
//! Request parsing is intentionally tiny: one method, one route. Any
//! ambiguity is answered with a rejection rather than a guess.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

/// Cap on the request head we will buffer. A client that sends more
/// than this before `\r\n\r\n` is hung up on rather than accommodated.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;

pub const STATUS_PATH: &str = "/status";

/// One contributor's standing in the PPLNS window.
///
/// Keyed by payout script, because that is what the window and the
/// coinbase split are keyed by. The per-miner share COUNTERS live in
/// `Ledger`, which is keyed by Noise pubkey instead — the two cannot be
/// joined without inventing an identity mapping, so counters are
/// reported pool-wide on `OpsStatus` rather than guessed at per miner.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MinerStatus {
    /// Hex of the payout script this contributor is credited under.
    pub payout_script_hex: String,
    /// Share of the next block's contributor split, in basis points.
    pub bps: u32,
    /// Summed difficulty weight currently inside the window.
    pub window_weight: String,
}

/// Everything the endpoint reports. Purely descriptive — a consumer
/// that wants *earnings* should read the chain, not this.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpsStatus {
    pub pool_version: String,
    pub uptime_secs: u64,
    /// Operator fee in basis points (1000 = 10%).
    pub fee_bps: u32,
    pub connected_miners: usize,
    /// PPLNS window depth and the wall-clock span it covers.
    pub window_entries: usize,
    pub window_span_secs: u64,
    pub template_heartbeat_age_secs: u64,
    pub template_phase: String,
    /// Pool-wide since process start (the Ledger is in-memory only).
    pub accepted_shares_total: u64,
    pub rejected_shares_total: u64,
    pub blocks_found_total: u64,
    pub miners: Vec<MinerStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub bearer: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Serve,
    Unauthorized,
    NotFound,
    MethodNotAllowed,
    BadRequest,
}

impl Decision {
    pub fn status_line(self) -> &'static str {
        match self {
            Decision::Serve => "200 OK",
            Decision::Unauthorized => "401 Unauthorized",
            Decision::NotFound => "404 Not Found",
            Decision::MethodNotAllowed => "405 Method Not Allowed",
            Decision::BadRequest => "400 Bad Request",
        }
    }
}

/// Length-independent byte comparison.
///
/// Returns early ONLY on length, which is not secret here (the token
/// length is fixed by whoever generated it). Content comparison always
/// touches every byte so a timing signal can't walk the token.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Parse the request head. `None` for anything malformed — callers turn
/// that into 400 rather than trying to recover meaning.
pub fn parse_request(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let mut start = lines.next()?.split_whitespace();
    let method = start.next()?.to_string();
    let raw_path = start.next()?;
    // Ignore any query string; no route takes parameters.
    let path = raw_path.split('?').next().unwrap_or(raw_path).to_string();

    let mut bearer = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("authorization") {
            let v = value.trim();
            // Scheme is case-insensitive per RFC 7235.
            if v.len() >= 7 && v[..7].eq_ignore_ascii_case("bearer ") {
                bearer = Some(v[7..].trim().to_string());
            }
        }
    }
    Some(Request { method, path, bearer })
}

/// Authorize and route. Auth is checked BEFORE the path, so an
/// unauthenticated caller cannot probe which routes exist.
pub fn decide(req: &Request, expected_token: &str) -> Decision {
    // An empty configured token would otherwise match an empty
    // presented one and authorize the world. Fail closed; the server
    // also refuses to start without a token.
    if expected_token.is_empty() {
        return Decision::Unauthorized;
    }
    // Auth BEFORE routing: a caller with a bad token must not be able
    // to tell a real route from a bogus one.
    let authorized = match req.bearer.as_deref() {
        Some(token) => constant_time_eq(token.as_bytes(), expected_token.as_bytes()),
        None => false,
    };
    if !authorized {
        return Decision::Unauthorized;
    }
    if req.path != STATUS_PATH {
        return Decision::NotFound;
    }
    match req.method.as_str() {
        "GET" | "HEAD" => Decision::Serve,
        _ => Decision::MethodNotAllowed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(method: &str, path: &str, bearer: Option<&str>) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            bearer: bearer.map(|s| s.to_string()),
        }
    }

    // ---- auth ----

    #[test]
    fn correct_token_on_the_status_route_is_served() {
        assert_eq!(decide(&req("GET", STATUS_PATH, Some("s3cret")), "s3cret"), Decision::Serve);
    }

    #[test]
    fn missing_token_is_rejected() {
        assert_eq!(decide(&req("GET", STATUS_PATH, None), "s3cret"), Decision::Unauthorized);
    }

    #[test]
    fn wrong_token_is_rejected() {
        assert_eq!(
            decide(&req("GET", STATUS_PATH, Some("guess")), "s3cret"),
            Decision::Unauthorized
        );
    }

    #[test]
    fn auth_is_checked_before_routing_so_routes_cannot_be_probed() {
        // An unauthenticated caller must not learn which paths exist:
        // a bad token on a bogus path must look like a bad token on a
        // real one.
        assert_eq!(decide(&req("GET", "/nope", Some("guess")), "s3cret"), Decision::Unauthorized);
        assert_eq!(
            decide(&req("GET", STATUS_PATH, Some("guess")), "s3cret"),
            Decision::Unauthorized
        );
    }

    #[test]
    fn authenticated_unknown_route_is_not_found() {
        assert_eq!(decide(&req("GET", "/nope", Some("s3cret")), "s3cret"), Decision::NotFound);
    }

    #[test]
    fn writes_are_refused_even_with_a_valid_token() {
        // The endpoint is read-only by contract, not just by omission.
        for m in ["POST", "PUT", "DELETE", "PATCH"] {
            assert_eq!(
                decide(&req(m, STATUS_PATH, Some("s3cret")), "s3cret"),
                Decision::MethodNotAllowed,
                "{m} should be refused"
            );
        }
    }

    #[test]
    fn head_is_treated_as_a_read_and_allowed() {
        assert_eq!(decide(&req("HEAD", STATUS_PATH, Some("s3cret")), "s3cret"), Decision::Serve);
    }

    #[test]
    fn an_empty_configured_token_authorizes_nobody() {
        // Fail closed: an empty token file must not turn into
        // "" == "" and open the endpoint.
        assert_eq!(decide(&req("GET", STATUS_PATH, Some("")), ""), Decision::Unauthorized);
        assert_eq!(decide(&req("GET", STATUS_PATH, None), ""), Decision::Unauthorized);
        assert_eq!(decide(&req("GET", STATUS_PATH, Some("anything")), ""), Decision::Unauthorized);
    }

    // ---- constant-time compare ----

    #[test]
    fn constant_time_eq_matches_only_identical_bytes() {
        assert!(constant_time_eq(b"abc123", b"abc123"));
        assert!(!constant_time_eq(b"abc123", b"abc124"));
        assert!(!constant_time_eq(b"abc123", b"abc12"));
        assert!(!constant_time_eq(b"", b"x"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn a_token_sharing_a_long_prefix_is_still_rejected() {
        assert!(!constant_time_eq(
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaX",
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaY"
        ));
    }

    // ---- parsing ----

    #[test]
    fn parses_method_path_and_bearer() {
        let head = "GET /status HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer tok123\r\n\r\n";
        let r = parse_request(head).unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/status");
        assert_eq!(r.bearer.as_deref(), Some("tok123"));
    }

    #[test]
    fn header_and_scheme_names_are_case_insensitive() {
        let head = "GET /status HTTP/1.1\r\nauthorization: bEaReR tok123\r\n\r\n";
        assert_eq!(parse_request(head).unwrap().bearer.as_deref(), Some("tok123"));
    }

    #[test]
    fn query_string_is_stripped_from_the_route() {
        let head = "GET /status?verbose=1 HTTP/1.1\r\nAuthorization: Bearer t\r\n\r\n";
        assert_eq!(parse_request(head).unwrap().path, "/status");
    }

    #[test]
    fn a_non_bearer_authorization_scheme_yields_no_token() {
        let head = "GET /status HTTP/1.1\r\nAuthorization: Basic dXNlcjpwdw==\r\n\r\n";
        assert_eq!(parse_request(head).unwrap().bearer, None);
    }

    #[test]
    fn garbage_does_not_parse() {
        assert!(parse_request("").is_none());
        assert!(parse_request("GET\r\n\r\n").is_none());
    }
}

/// Read the request head (up to `\r\n\r\n`), refusing anything larger
/// than `MAX_HEAD_BYTES`. Returns `None` on EOF/oversize/timeout.
async fn read_head(sock: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let n = match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => return None,
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return None,
        };
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_HEAD_BYTES {
            return None;
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(buf).ok()
}

async fn respond(sock: &mut TcpStream, decision: Decision, body: &str, head_only: bool) {
    let payload = if head_only { "" } else { body };
    let response = format!(
        "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{}",
        decision.status_line(),
        body.len(),
        payload
    );
    let _ = sock.write_all(response.as_bytes()).await;
    let _ = sock.shutdown().await;
}

/// Serve the read-only status endpoint until the listener dies.
///
/// `snapshot` is called only for authorized requests, so an unauthorized
/// caller cannot make the pool do work.
pub async fn serve<F>(listener: TcpListener, token: String, snapshot: Arc<F>) -> Result<()>
where
    F: Fn() -> OpsStatus + Send + Sync + 'static,
{
    if token.is_empty() {
        anyhow::bail!("ops endpoint refuses to start without a token");
    }
    loop {
        let (mut sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "ops accept failed");
                continue;
            }
        };
        let token = token.clone();
        let snapshot = snapshot.clone();
        tokio::spawn(async move {
            let Some(head) = read_head(&mut sock).await else {
                respond(&mut sock, Decision::BadRequest, "{\"error\":\"bad request\"}", false).await;
                return;
            };
            let Some(req) = parse_request(&head) else {
                respond(&mut sock, Decision::BadRequest, "{\"error\":\"bad request\"}", false).await;
                return;
            };
            let head_only = req.method.eq_ignore_ascii_case("HEAD");
            match decide(&req, &token) {
                Decision::Serve => {
                    let body = serde_json::to_string(&snapshot())
                        .unwrap_or_else(|_| "{\"error\":\"encode failed\"}".to_string());
                    respond(&mut sock, Decision::Serve, &body, head_only).await;
                }
                other => {
                    warn!(%peer, path = %req.path, status = other.status_line(), "ops request refused");
                    respond(&mut sock, other, "{\"error\":\"refused\"}", head_only).await;
                }
            }
        });
    }
}

/// Load the bearer token from a file, rejecting an empty one.
pub fn load_token(path: &std::path::Path) -> Result<String> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading ops token {path:?}"))?;
    let token = raw.trim().to_string();
    if token.is_empty() {
        anyhow::bail!("ops token file {path:?} is empty");
    }
    Ok(token)
}

/// Bind the ops listener, warning loudly if it is exposed beyond loopback.
pub async fn bind(addr: &str) -> Result<TcpListener> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding ops endpoint on {addr}"))?;
    if !addr.starts_with("127.0.0.1:") && !addr.starts_with("localhost:") {
        warn!(
            %addr,
            "ops endpoint is NOT on loopback — it speaks plain HTTP, so put a TLS \
             reverse proxy in front of it or tunnel over SSH"
        );
    }
    info!(%addr, "ops endpoint listening (read-only)");
    Ok(listener)
}
