//! Read-only operator status endpoint.
//!
//! Exists so a pool operator can see what their pool is doing without
//! shelling into the box — and so `dinero-qt` can show it.
//!
//! Deliberate constraints:
//!
//!   * **Read-only by default.** `/status` mutates nothing, and the
//!     endpoint can never move coins, change the fee rate, or touch the
//!     PPLNS window.
//!
//!     The one exception is opt-in: `POST /payout-address`, enabled only
//!     by `--ops-allow-payout-change`, retargets the operator's own fee
//!     output. It is OFF unless an operator turns it on, because enabling
//!     it upgrades the ops token from a *read* credential to one that can
//!     redirect the operator's share of every future block. Nothing about
//!     it can touch a *miner's* payout — contributor outputs come from the
//!     PPLNS window, which no route reaches.
//!
//!     A candidate address is proven against a real `getblocktemplate`
//!     before it is adopted, so a typo is rejected rather than silently
//!     killing template production.
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
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

/// Cap on the request head we will buffer. A client that sends more
/// than this before `\r\n\r\n` is hung up on rather than accommodated.
pub const MAX_HEAD_BYTES: usize = 8 * 1024;

pub const STATUS_PATH: &str = "/status";

/// Mutating route: set the operator's fee/coinbase address at runtime.
/// Gated behind `Policy::allow_payout_change` — OFF by default, because
/// reaching it means the ops token can redirect money.
pub const PAYOUT_PATH: &str = "/payout-address";

/// Cap on a request body. The only body we accept is a one-field JSON
/// object holding an address, so anything larger is a mistake or an attack.
pub const MAX_BODY_BYTES: usize = 1024;

/// What the endpoint is permitted to do, decided once at startup from CLI
/// flags. Defaults to the historical posture: read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Policy {
    /// `--ops-allow-payout-change`. When false the payout route 403s even
    /// for a caller holding the correct token.
    pub allow_payout_change: bool,
}

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentShare {
    pub accepted_at_unix: u64,
    pub kind: String,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentBlock {
    pub observed_at_unix: u64,
    pub status: String,
    pub hash: String,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct TelemetryState {
    pub accepted: u64,
    pub rejected: u64,
    pub rejection_reasons: BTreeMap<String, u64>,
    pub last_share: Option<RecentShare>,
    pub last_block: Option<RecentBlock>,
    pub last_template_at_unix: u64,
    pub last_template_height: u64,
    pub last_template_id: u64,
    pub last_template_hash: String,
    pub daemon_endpoint: String,
    pub daemon_blocks: u64,
    pub daemon_headers: u64,
}

#[derive(Debug, Default)]
pub struct OpsTelemetry(Mutex<TelemetryState>);

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl OpsTelemetry {
    pub fn record_template(
        &self,
        endpoint: &str,
        blocks: u64,
        headers: u64,
        height: u64,
        template_id: u64,
        prev_hash: String,
    ) {
        let mut s = self.0.lock().expect("ops telemetry mutex");
        s.last_template_at_unix = now_unix();
        s.last_template_height = height;
        s.last_template_id = template_id;
        s.last_template_hash = prev_hash;
        s.daemon_endpoint = endpoint.to_owned();
        s.daemon_blocks = blocks;
        s.daemon_headers = headers;
    }

    pub fn record_accepted_share(&self, kind: &str, hash: String) {
        let mut s = self.0.lock().expect("ops telemetry mutex");
        s.accepted = s.accepted.saturating_add(1);
        s.last_share = Some(RecentShare {
            accepted_at_unix: now_unix(),
            kind: kind.to_owned(),
            hash,
        });
    }

    pub fn record_rejection(&self, reason: &str) {
        let mut s = self.0.lock().expect("ops telemetry mutex");
        s.rejected = s.rejected.saturating_add(1);
        *s.rejection_reasons.entry(reason.to_owned()).or_default() += 1;
    }

    pub fn record_block(&self, status: &str, hash: String, reason: String) {
        self.0.lock().expect("ops telemetry mutex").last_block = Some(RecentBlock {
            observed_at_unix: now_unix(),
            status: status.to_owned(),
            hash,
            reason,
        });
    }

    pub fn snapshot(&self) -> TelemetryState {
        let s = self.0.lock().expect("ops telemetry mutex");
        TelemetryState {
            accepted: s.accepted,
            rejected: s.rejected,
            rejection_reasons: s.rejection_reasons.clone(),
            last_share: s.last_share.clone(),
            last_block: s.last_block.clone(),
            last_template_at_unix: s.last_template_at_unix,
            last_template_height: s.last_template_height,
            last_template_id: s.last_template_id,
            last_template_hash: s.last_template_hash.clone(),
            daemon_endpoint: s.daemon_endpoint.clone(),
            daemon_blocks: s.daemon_blocks,
            daemon_headers: s.daemon_headers,
        }
    }
}

static TELEMETRY: OnceLock<OpsTelemetry> = OnceLock::new();

pub fn telemetry() -> &'static OpsTelemetry {
    TELEMETRY.get_or_init(OpsTelemetry::default)
}

/// Everything the endpoint reports. Purely descriptive — a consumer
/// that wants *earnings* should read the chain, not this.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpsStatus {
    /// Monotonic contract version for strict consumers. Fields from v1 remain
    /// present so older Qt releases continue to work.
    pub schema_version: u32,
    pub generated_at_unix: u64,
    pub pool_version: String,
    pub uptime_secs: u64,
    /// Operator fee in basis points (1000 = 10%).
    pub fee_bps: u32,
    /// The address currently receiving the operator fee. Reported so an
    /// operator can confirm what is live rather than trusting the unit file.
    /// Not a secret: it appears in the coinbase of every block found.
    pub payout_address: String,
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
    pub stratum_bind: String,
    pub daemon_connected: bool,
    pub daemon_endpoint: String,
    pub daemon_blocks: u64,
    pub daemon_headers: u64,
    pub template_height: u64,
    pub template_id: u64,
    pub template_prev_hash: String,
    pub last_template_at_unix: u64,
    pub last_share: Option<RecentShare>,
    pub last_block: Option<RecentBlock>,
    pub rejection_reasons: BTreeMap<String, u64>,
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
    /// Authenticated, route exists, but the operator did not enable it.
    Forbidden,
}

impl Decision {
    pub fn status_line(self) -> &'static str {
        match self {
            Decision::Serve => "200 OK",
            Decision::Unauthorized => "401 Unauthorized",
            Decision::NotFound => "404 Not Found",
            Decision::MethodNotAllowed => "405 Method Not Allowed",
            Decision::BadRequest => "400 Bad Request",
            Decision::Forbidden => "403 Forbidden",
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
    Some(Request {
        method,
        path,
        bearer,
    })
}

/// Authorize and route. Auth is checked BEFORE the path, so an
/// unauthenticated caller cannot probe which routes exist.
pub fn decide(req: &Request, expected_token: &str, policy: Policy) -> Decision {
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
    match req.path.as_str() {
        STATUS_PATH => match req.method.as_str() {
            "GET" | "HEAD" => Decision::Serve,
            // Enabling the payout route must not make /status writable.
            _ => Decision::MethodNotAllowed,
        },
        PAYOUT_PATH => {
            // Method before policy: a GET here is wrong regardless of whether
            // the operator enabled changes, and answering 405 rather than 403
            // keeps the two mistakes distinguishable in an operator's logs.
            if req.method != "POST" {
                return Decision::MethodNotAllowed;
            }
            if !policy.allow_payout_change {
                return Decision::Forbidden;
            }
            Decision::Serve
        }
        _ => Decision::NotFound,
    }
}

/// Extract the address from a `{"address": "din1p..."}` body.
pub fn parse_payout_body(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let addr = v.get("address")?.as_str()?.trim();
    if addr.is_empty() {
        return None;
    }
    Some(addr.to_string())
}

/// Cheap syntactic gate so an obvious typo never costs an RPC round-trip.
/// Deliberately NOT a full bech32m decode — the authoritative check is the
/// trial `getblocktemplate`, which is the thing that actually has to succeed.
pub fn looks_like_payout_address(s: &str) -> bool {
    // Bech32(m) data charset. Excludes `1`, `b`, `i`, `o` by design, which is
    // what rejects a stray separator, whitespace, or a second address.
    const CHARSET: &[u8] = b"qpzry9x8gf2tvdw0s3jn54khce6mua7l";
    const MIN: usize = 20;
    const MAX: usize = 90;

    if !s.starts_with("din1p") || s.len() < MIN || s.len() > MAX {
        return false;
    }
    // `din1` is the hrp + separator; everything after must be data charset.
    s.as_bytes()[4..].iter().all(|b| CHARSET.contains(b))
}

/// `Content-Length` from a request head, if present and sane.
pub fn content_length(head: &str) -> Option<usize> {
    for line in head.split("\r\n") {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let n: usize = value.trim().parse().ok()?;
            // Refuse rather than clamp: a caller announcing more than we will
            // ever accept is not sending something we want to half-read.
            return (n <= MAX_BODY_BYTES).then_some(n);
        }
    }
    None
}

/// Split a buffer at the head/body boundary. `read_head` can over-read into
/// the body, so those bytes must be carried forward rather than dropped.
pub fn split_head_body(buf: &[u8]) -> Option<(String, Vec<u8>)> {
    let at = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let split = at + 4;
    let head = String::from_utf8(buf[..split].to_vec()).ok()?;
    Some((head, buf[split..].to_vec()))
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

    // ---- payout route: gating ----
    //
    // The whole point of the flag is that enabling a money-routing verb is a
    // deliberate act. Default-constructed Policy must refuse.

    fn open() -> Policy {
        Policy {
            allow_payout_change: true,
        }
    }

    #[test]
    fn payout_change_is_refused_when_not_enabled() {
        assert_eq!(
            decide(
                &req("POST", PAYOUT_PATH, Some("s3cret")),
                "s3cret",
                Policy::default()
            ),
            Decision::Forbidden
        );
    }

    #[test]
    fn payout_change_is_served_when_enabled() {
        assert_eq!(
            decide(&req("POST", PAYOUT_PATH, Some("s3cret")), "s3cret", open()),
            Decision::Serve
        );
    }

    // Auth still comes first: a caller with a bad token must not be able to
    // tell whether this pool has payout-changing switched on.
    #[test]
    fn payout_route_reports_unauthorized_before_forbidden() {
        assert_eq!(
            decide(
                &req("POST", PAYOUT_PATH, Some("wrong")),
                "s3cret",
                Policy::default()
            ),
            Decision::Unauthorized
        );
        assert_eq!(
            decide(&req("POST", PAYOUT_PATH, Some("wrong")), "s3cret", open()),
            Decision::Unauthorized
        );
    }

    // Reading the address is what /status is for; this route only writes.
    #[test]
    fn payout_route_rejects_non_post() {
        for m in ["GET", "HEAD", "PUT", "DELETE"] {
            assert_eq!(
                decide(&req(m, PAYOUT_PATH, Some("s3cret")), "s3cret", open()),
                Decision::MethodNotAllowed,
                "method {m}"
            );
        }
    }

    // Enabling payout changes must not turn /status into a writable route.
    #[test]
    fn enabling_payout_change_does_not_make_status_writable() {
        assert_eq!(
            decide(&req("POST", STATUS_PATH, Some("s3cret")), "s3cret", open()),
            Decision::MethodNotAllowed
        );
    }

    #[test]
    fn unknown_path_is_still_not_found_when_payout_is_enabled() {
        assert_eq!(
            decide(&req("POST", "/withdraw", Some("s3cret")), "s3cret", open()),
            Decision::NotFound
        );
    }

    #[test]
    fn forbidden_renders_403() {
        assert_eq!(Decision::Forbidden.status_line(), "403 Forbidden");
    }

    // ---- body parsing ----

    #[test]
    fn body_yields_the_address() {
        assert_eq!(
            parse_payout_body(r#"{"address":"din1pabc"}"#).as_deref(),
            Some("din1pabc")
        );
    }

    #[test]
    fn body_tolerates_whitespace_and_ordering() {
        assert_eq!(
            parse_payout_body("{ \"note\": \"x\", \"address\" : \"  din1pabc  \" }").as_deref(),
            Some("din1pabc")
        );
    }

    #[test]
    fn body_without_an_address_is_rejected() {
        for b in [
            r#"{}"#,
            r#"{"addr":"din1pabc"}"#,
            r#"{"address":null}"#,
            r#"{"address":123}"#,
            r#"{"address":""}"#,
            r#"{"address":"   "}"#,
            "not json",
            "",
        ] {
            assert!(parse_payout_body(b).is_none(), "should reject: {b}");
        }
    }

    // ---- syntactic address gate ----

    #[test]
    fn plausible_addresses_pass_the_cheap_gate() {
        assert!(looks_like_payout_address(
            "din1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggxy"
        ));
    }

    #[test]
    fn obvious_junk_is_rejected_without_an_rpc() {
        for bad in [
            "",
            "din1q0000000000000000000000000000000000000000000000000000000000", // wrong prefix
            "din1p",                                                           // too short
            "bc1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggxy",  // other chain
            "din1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggx!", // bad charset
            "din1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggxy din1pother", // two
        ] {
            assert!(!looks_like_payout_address(bad), "should reject: {bad:?}");
        }
    }

    // A dangerously long value must not reach the RPC layer.
    #[test]
    fn absurdly_long_address_is_rejected() {
        let long = format!("din1p{}", "q".repeat(4096));
        assert!(!looks_like_payout_address(&long));
    }

    // ---- head/body framing ----

    #[test]
    fn content_length_is_read_case_insensitively() {
        assert_eq!(
            content_length("POST / HTTP/1.1\r\ncontent-length: 22\r\n\r\n"),
            Some(22)
        );
        assert_eq!(
            content_length("POST / HTTP/1.1\r\nContent-Length:  7 \r\n\r\n"),
            Some(7)
        );
        assert_eq!(content_length("POST / HTTP/1.1\r\nHost: x\r\n\r\n"), None);
        assert_eq!(
            content_length("POST / HTTP/1.1\r\nContent-Length: nope\r\n\r\n"),
            None
        );
    }

    #[test]
    fn oversize_content_length_is_refused() {
        let h = format!(
            "POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        );
        assert_eq!(content_length(&h), None);
    }

    // read_head reads in 512-byte chunks, so it routinely swallows the first
    // bytes of the body. Dropping them corrupts every POST.
    #[test]
    fn split_carries_over_read_body_bytes_forward() {
        let raw = b"POST /payout-address HTTP/1.1\r\nContent-Length: 9\r\n\r\n{\"a\":1}!!";
        let (head, body) = split_head_body(raw).expect("splits");
        assert!(head.starts_with("POST /payout-address"));
        assert!(head.ends_with("\r\n\r\n"));
        assert_eq!(body, b"{\"a\":1}!!");
    }

    #[test]
    fn split_returns_empty_body_when_none_was_sent() {
        let raw = b"GET /status HTTP/1.1\r\nHost: x\r\n\r\n";
        let (_, body) = split_head_body(raw).expect("splits");
        assert!(body.is_empty());
    }

    #[test]
    fn split_refuses_a_head_with_no_terminator() {
        assert!(split_head_body(b"GET /status HTTP/1.1\r\nHost: x").is_none());
    }

    // ---- auth ----

    #[test]
    fn correct_token_on_the_status_route_is_served() {
        assert_eq!(
            decide(
                &req("GET", STATUS_PATH, Some("s3cret")),
                "s3cret",
                Policy::default()
            ),
            Decision::Serve
        );
    }

    #[test]
    fn missing_token_is_rejected() {
        assert_eq!(
            decide(&req("GET", STATUS_PATH, None), "s3cret", Policy::default()),
            Decision::Unauthorized
        );
    }

    #[test]
    fn wrong_token_is_rejected() {
        assert_eq!(
            decide(
                &req("GET", STATUS_PATH, Some("guess")),
                "s3cret",
                Policy::default()
            ),
            Decision::Unauthorized
        );
    }

    #[test]
    fn auth_is_checked_before_routing_so_routes_cannot_be_probed() {
        // An unauthenticated caller must not learn which paths exist:
        // a bad token on a bogus path must look like a bad token on a
        // real one.
        assert_eq!(
            decide(
                &req("GET", "/nope", Some("guess")),
                "s3cret",
                Policy::default()
            ),
            Decision::Unauthorized
        );
        assert_eq!(
            decide(
                &req("GET", STATUS_PATH, Some("guess")),
                "s3cret",
                Policy::default()
            ),
            Decision::Unauthorized
        );
    }

    #[test]
    fn authenticated_unknown_route_is_not_found() {
        assert_eq!(
            decide(
                &req("GET", "/nope", Some("s3cret")),
                "s3cret",
                Policy::default()
            ),
            Decision::NotFound
        );
    }

    #[test]
    fn writes_are_refused_even_with_a_valid_token() {
        // The endpoint is read-only by contract, not just by omission.
        for m in ["POST", "PUT", "DELETE", "PATCH"] {
            assert_eq!(
                decide(
                    &req(m, STATUS_PATH, Some("s3cret")),
                    "s3cret",
                    Policy::default()
                ),
                Decision::MethodNotAllowed,
                "{m} should be refused"
            );
        }
    }

    #[test]
    fn head_is_treated_as_a_read_and_allowed() {
        assert_eq!(
            decide(
                &req("HEAD", STATUS_PATH, Some("s3cret")),
                "s3cret",
                Policy::default()
            ),
            Decision::Serve
        );
    }

    #[test]
    fn an_empty_configured_token_authorizes_nobody() {
        // Fail closed: an empty token file must not turn into
        // "" == "" and open the endpoint.
        assert_eq!(
            decide(&req("GET", STATUS_PATH, Some("")), "", Policy::default()),
            Decision::Unauthorized
        );
        assert_eq!(
            decide(&req("GET", STATUS_PATH, None), "", Policy::default()),
            Decision::Unauthorized
        );
        assert_eq!(
            decide(
                &req("GET", STATUS_PATH, Some("anything")),
                "",
                Policy::default()
            ),
            Decision::Unauthorized
        );
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

    #[test]
    fn telemetry_preserves_last_events_and_groups_rejections() {
        let telemetry = OpsTelemetry::default();
        telemetry.record_template("http://127.0.0.1:20998", 42, 43, 43, 9, "ab".repeat(32));
        telemetry.record_accepted_share("shared", "cd".repeat(32));
        telemetry.record_rejection("stale-share");
        telemetry.record_rejection("stale-share");
        telemetry.record_block("rejected", "ef".repeat(32), "bad-root".to_string());

        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.accepted, 1);
        assert_eq!(snapshot.rejected, 2);
        assert_eq!(snapshot.rejection_reasons.get("stale-share"), Some(&2));
        assert_eq!(snapshot.daemon_blocks, 42);
        assert_eq!(snapshot.daemon_headers, 43);
        assert_eq!(snapshot.last_template_height, 43);
        assert_eq!(snapshot.last_share.unwrap().kind, "shared");
        assert_eq!(snapshot.last_block.unwrap().reason, "bad-root");
    }

    // ---- parsing ----

    #[test]
    fn parses_method_path_and_bearer() {
        let head =
            "GET /status HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer tok123\r\n\r\n";
        let r = parse_request(head).unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path, "/status");
        assert_eq!(r.bearer.as_deref(), Some("tok123"));
    }

    #[test]
    fn header_and_scheme_names_are_case_insensitive() {
        let head = "GET /status HTTP/1.1\r\nauthorization: bEaReR tok123\r\n\r\n";
        assert_eq!(
            parse_request(head).unwrap().bearer.as_deref(),
            Some("tok123")
        );
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
async fn read_head(sock: &mut TcpStream) -> Option<Vec<u8>> {
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
    Some(buf)
}

/// Read exactly `want` body bytes, reusing whatever `read_head` over-read.
async fn read_body(sock: &mut TcpStream, mut have: Vec<u8>, want: usize) -> Option<String> {
    if want > MAX_BODY_BYTES {
        return None;
    }
    let mut chunk = [0u8; 512];
    while have.len() < want {
        let n = match tokio::time::timeout(Duration::from_secs(5), sock.read(&mut chunk)).await {
            Ok(Ok(0)) | Err(_) => return None,
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return None,
        };
        have.extend_from_slice(&chunk[..n]);
        if have.len() > MAX_BODY_BYTES {
            return None;
        }
    }
    have.truncate(want);
    String::from_utf8(have).ok()
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
pub async fn serve<F, A, Fut>(
    listener: TcpListener,
    token: String,
    policy: Policy,
    snapshot: Arc<F>,
    apply_payout: Arc<A>,
) -> Result<()>
where
    F: Fn() -> OpsStatus + Send + Sync + 'static,
    A: Fn(String) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = std::result::Result<String, String>> + Send,
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
        let apply_payout = apply_payout.clone();
        tokio::spawn(async move {
            let Some(raw) = read_head(&mut sock).await else {
                respond(
                    &mut sock,
                    Decision::BadRequest,
                    "{\"error\":\"bad request\"}",
                    false,
                )
                .await;
                return;
            };
            let Some((head, body_prefix)) = split_head_body(&raw) else {
                respond(
                    &mut sock,
                    Decision::BadRequest,
                    "{\"error\":\"bad request\"}",
                    false,
                )
                .await;
                return;
            };
            let Some(req) = parse_request(&head) else {
                respond(
                    &mut sock,
                    Decision::BadRequest,
                    "{\"error\":\"bad request\"}",
                    false,
                )
                .await;
                return;
            };
            let head_only = req.method.eq_ignore_ascii_case("HEAD");
            match decide(&req, &token, policy) {
                Decision::Serve if req.path == PAYOUT_PATH => {
                    let Some(want) = content_length(&head) else {
                        respond(
                            &mut sock,
                            Decision::BadRequest,
                            "{\"error\":\"Content-Length required, and must not exceed 1024\"}",
                            false,
                        )
                        .await;
                        return;
                    };
                    let Some(body) = read_body(&mut sock, body_prefix, want).await else {
                        respond(
                            &mut sock,
                            Decision::BadRequest,
                            "{\"error\":\"short body\"}",
                            false,
                        )
                        .await;
                        return;
                    };
                    let Some(addr) = parse_payout_body(&body) else {
                        respond(
                            &mut sock,
                            Decision::BadRequest,
                            "{\"error\":\"expected {\\\"address\\\": \\\"din1p...\\\"}\"}",
                            false,
                        )
                        .await;
                        return;
                    };
                    // Logged unconditionally: a change of where money goes is
                    // the one thing an operator must be able to audit later.
                    info!(%peer, candidate = %addr, "ops payout-address change requested");
                    match apply_payout(addr).await {
                        Ok(applied) => {
                            info!(%peer, address = %applied, "ops payout address CHANGED");
                            let body = serde_json::json!({
                                "ok": true,
                                "payout_address": applied
                            })
                            .to_string();
                            respond(&mut sock, Decision::Serve, &body, false).await;
                        }
                        Err(why) => {
                            warn!(%peer, error = %why, "ops payout-address change REFUSED");
                            let body = serde_json::json!({ "ok": false, "error": why }).to_string();
                            respond(&mut sock, Decision::BadRequest, &body, false).await;
                        }
                    }
                }
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
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading ops token {path:?}"))?;
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
