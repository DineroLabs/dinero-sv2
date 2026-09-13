//! Public, unauthenticated, READ-ONLY pool statistics.
//!
//! A second HTTP listener, entirely separate from the operator endpoint in
//! `ops.rs`. It exists so aggregator sites (MiningPoolStats, minerstat) and
//! a browser landing on the pool's hostname can see hashrate, miners and
//! found blocks without a token. Because anyone on the internet can reach
//! it, its contract is deliberately narrow:
//!
//!   * **No secrets, no mutation.** Nothing here can move coins, change a
//!     fee, or read the operator's payout address, token, daemon endpoint,
//!     or configuration. There is no route that accepts a body.
//!   * **No shared code path with ops routing.** `serve` has its own router
//!     that knows only `/`, `/api/stats`, `/api/blocks` and `/api/miner/…`;
//!     every ops path is a 404 here by construction, and `/api/*` is a 404
//!     on the ops listener. `tests/public_stats.rs` pins both directions.
//!   * **No enumeration.** `/api/miner/<addr>` answers only for an address
//!     that has actually submitted a share; there is no list of miners.
//!   * **Cheap to hit.** One sample of pool state is taken at most every
//!     `Config::cache_ttl` (10s by default) and rendered ONCE — the stats
//!     JSON, the recent-block list and a per-address aggregate map — so a
//!     request is a clone or a hash lookup, never a walk of the PPLNS
//!     window. The sample is built outside the cache lock. On top of that:
//!     a fixed-window per-client rate limit (keyed by the first hop of
//!     `X-Forwarded-For`/`X-Real-IP` when the peer is loopback, i.e. a
//!     local reverse proxy, otherwise by the peer; IPv6 by /64; the key
//!     table is hard-capped), a cap on concurrent connections, and a
//!     deadline on every connection from accept to close.
//!   * **Cross-origin readable.** `Access-Control-Allow-Origin: *` on GET,
//!     so a static web page anywhere can render it.
//!
//! Everything derived (hashrate, active miners, per-miner estimates) is
//! computed by pure functions over a `Sample`, so the arithmetic is unit
//! tested without a socket.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Write as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tracing::{info, warn};

use crate::accounting::WindowEntry;

/// Shares inside this many seconds count toward the reported hashrate.
pub const HASHRATE_WINDOW_SECS: u64 = 600;
/// An address with a share inside this many seconds is an active miner.
pub const ACTIVE_MINER_SECS: u64 = 600;
const HOUR_SECS: u64 = 3_600;
const DAY_SECS: u64 = 86_400;
/// `?limit=` bounds for `/api/blocks`.
pub const DEFAULT_BLOCKS_LIMIT: usize = 50;
pub const MAX_BLOCKS_LIMIT: usize = 100;
/// How many found blocks are kept in memory for `/api/blocks`. The total
/// count is tracked separately, so it is not capped by this.
pub const MAX_BLOCK_HISTORY: usize = 1_000;
/// How long one sample of pool state is served before it is rebuilt.
pub const DEFAULT_CACHE_TTL: Duration = Duration::from_secs(10);
/// Basename of the found-blocks log, kept next to the PPLNS journal.
pub const BLOCK_LOG_BASENAME: &str = "found-blocks.jsonl";

const STATS_PATH: &str = "/api/stats";
const BLOCKS_PATH: &str = "/api/blocks";
const MINER_PREFIX: &str = "/api/miner/";

const INDEX_HTML: &str = include_str!("stats_index.html");

// ---------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------

/// One block this pool found and dinerod accepted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FoundBlock {
    pub height: u64,
    pub hash: String,
    /// Unix time the pool observed acceptance.
    pub time: u64,
    /// Total coinbase value in una.
    pub reward_una: u64,
    pub status: String,
    /// `(payout script hex, una)` per coinbase output, when the pool built
    /// the coinbase (shared blocks). Internal: feeds `paid_una` on the
    /// per-miner route and is NOT emitted by `/api/blocks`.
    #[serde(default)]
    pub outputs: Vec<(String, u64)>,
}

/// Raw pool state the binary hands over. Everything the API reports is
/// derived from this by pure functions below.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    pub now_unix: u64,
    /// The live PPLNS window, in credit order.
    pub window: Vec<WindowEntry>,
    /// Recent found blocks, oldest first.
    pub blocks: Vec<FoundBlock>,
    /// All blocks ever found, including ones no longer in `blocks`.
    pub blocks_found_total: u64,
    pub fee_bps: u32,
    /// Smallest contributor output the pool will emit (`--shared-dust-una`).
    pub min_payout_una: u64,
    /// The stratum endpoint miners should point at.
    pub stratum: String,
    pub network_height: u64,
    /// Compact target (nBits) of the current template.
    pub network_nbits: u32,
    /// Coinbase value of the current template, for per-miner estimates.
    pub next_reward_una: u64,
    /// Currently connected stratum sessions.
    pub connected_workers: usize,
}

// ---------------------------------------------------------------------
// Outputs (the public contract)
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PoolStats {
    /// SHARED-mode (PPLNS) hashrate: expected hashes/s from shares in the
    /// PPLNS window over the last 10 minutes. Solo-mode work is not
    /// credited to the window and is not included.
    pub pool_hashrate_hs: u64,
    /// Distinct payout addresses with a shared-mode share in the last 10
    /// minutes.
    pub miners: usize,
    /// Stratum sessions that completed the Noise handshake, shared and
    /// solo alike — so `workers` can exceed what `miners` accounts for.
    pub workers: usize,
    pub blocks_found_24h: u64,
    pub blocks_found_total: u64,
    pub last_block_height: Option<u64>,
    pub last_block_hash: Option<String>,
    pub last_block_time: Option<u64>,
    pub fee_bps: u32,
    pub payout_scheme: String,
    pub min_payout_una: u64,
    pub stratum: String,
    pub network_height: u64,
    pub network_difficulty: f64,
    pub updated_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BlockJson {
    pub height: u64,
    pub hash: String,
    pub time: u64,
    pub reward_una: u64,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MinerStats {
    pub address: String,
    pub hashrate_hs: u64,
    pub shares_1h: u64,
    /// Share of the next block's contributor split, in basis points.
    pub window_bps: u32,
    /// Estimate of what this address would receive if the NEXT block were
    /// found right now: next reward, less the operator fee, times
    /// `window_bps`. PPLNS holds no balance and owes nothing — this is a
    /// standing, not a debt, and it moves with every share anyone submits.
    pub est_next_block_una: u64,
    /// Una paid to this address in coinbases the pool built and dinerod
    /// accepted (since the pool started keeping its block log).
    pub paid_una: u64,
    pub last_share_time: Option<u64>,
}

// ---------------------------------------------------------------------
// Pure derivations
// ---------------------------------------------------------------------

/// Expected hashes per second represented by `entries` that fall inside
/// the hashrate window. `share_weight` is calibrated so one unit of weight
/// is one expected hash, so this is a sum divided by a span.
///
/// The span is the hashrate window, shortened only when the pool's whole
/// window is younger than that (a pool that started 90s ago has 90s of
/// evidence, not 600s).
fn hashrate_hs<'a>(
    now: u64,
    all: impl Iterator<Item = &'a WindowEntry>,
    mine: impl Iterator<Item = &'a WindowEntry>,
) -> u64 {
    let span = hashrate_span(now, all);
    recent_weight_hs(now, mine, span)
}

/// The divisor for hashrate: the hashrate window, or the age of the
/// pool's oldest window entry if that is shorter. Never zero.
fn hashrate_span<'a>(now: u64, all: impl Iterator<Item = &'a WindowEntry>) -> u64 {
    all.map(|e| e.unix_ts)
        .min()
        .map(|o| now.saturating_sub(o).min(HASHRATE_WINDOW_SECS))
        .unwrap_or(HASHRATE_WINDOW_SECS)
        .max(1)
}

fn recent_weight_hs<'a>(now: u64, mine: impl Iterator<Item = &'a WindowEntry>, span: u64) -> u64 {
    let cutoff = now.saturating_sub(HASHRATE_WINDOW_SECS);
    let sum: u128 = mine
        .filter(|e| e.unix_ts >= cutoff)
        .fold(0u128, |acc, e| acc.saturating_add(e.weight));
    u64::try_from(sum / u128::from(span.max(1))).unwrap_or(u64::MAX)
}

/// Bitcoin-convention difficulty for a compact target: how many times
/// harder than the `0x1d00ffff` difficulty-1 target. Aggregators expect
/// this number, whatever the chain's own genesis target was.
pub fn difficulty_from_nbits(nbits: u32) -> f64 {
    let exponent = (nbits >> 24) as i32;
    let mantissa = f64::from(nbits & 0x00ff_ffff);
    if mantissa == 0.0 {
        return 0.0;
    }
    (65_535.0 / mantissa) * 256f64.powi(29 - exponent)
}

pub fn compute_stats(s: &Sample) -> PoolStats {
    let now = s.now_unix;
    let active_cutoff = now.saturating_sub(ACTIVE_MINER_SECS);
    let miners = s
        .window
        .iter()
        .filter(|e| e.unix_ts >= active_cutoff)
        .map(|e| e.payout_script.as_slice())
        .collect::<HashSet<_>>()
        .len();
    let day_cutoff = now.saturating_sub(DAY_SECS);
    let last = s.blocks.last();
    PoolStats {
        pool_hashrate_hs: hashrate_hs(now, s.window.iter(), s.window.iter()),
        miners,
        workers: s.connected_workers,
        blocks_found_24h: s.blocks.iter().filter(|b| b.time >= day_cutoff).count() as u64,
        blocks_found_total: s.blocks_found_total,
        last_block_height: last.map(|b| b.height),
        last_block_hash: last.map(|b| b.hash.clone()),
        last_block_time: last.map(|b| b.time),
        fee_bps: s.fee_bps,
        payout_scheme: "PPLNS".to_string(),
        min_payout_una: s.min_payout_una,
        stratum: s.stratum.clone(),
        network_height: s.network_height,
        network_difficulty: difficulty_from_nbits(s.network_nbits),
        updated_at: now,
    }
}

/// Newest first, at most `limit`.
pub fn recent_blocks(s: &Sample, limit: usize) -> Vec<BlockJson> {
    s.blocks
        .iter()
        .rev()
        .take(limit)
        .map(|b| BlockJson {
            height: b.height,
            hash: b.hash.clone(),
            time: b.time,
            reward_una: b.reward_una,
            status: b.status.clone(),
        })
        .collect()
}

/// Per-address aggregate, built once per sample so `/api/miner/<addr>`
/// is a hash lookup rather than a walk of the window per request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ScriptAgg {
    weight: u128,
    recent_weight: u128,
    shares_1h: u64,
    last_share_time: Option<u64>,
    paid_una: u64,
}

/// One sample, fully rendered: everything a request can ask for, already
/// computed. Shared behind an `Arc` by every request within the TTL.
#[derive(Debug, Clone)]
pub struct Rendered {
    /// `/api/stats`, pre-encoded.
    pub stats_json: String,
    /// Newest first, already capped at `MAX_BLOCKS_LIMIT`.
    blocks: Vec<BlockJson>,
    per_script: HashMap<Vec<u8>, ScriptAgg>,
    total_weight: u128,
    span_secs: u64,
    fee_bps: u32,
    next_reward_una: u64,
}

/// One pass over the window and the block list.
pub fn render(s: &Sample) -> Rendered {
    let now = s.now_unix;
    let recent_cutoff = now.saturating_sub(HASHRATE_WINDOW_SECS);
    let hour_cutoff = now.saturating_sub(HOUR_SECS);
    let mut per_script: HashMap<Vec<u8>, ScriptAgg> = HashMap::new();
    let mut total_weight = 0u128;
    for e in &s.window {
        total_weight = total_weight.saturating_add(e.weight);
        let a = per_script.entry(e.payout_script.clone()).or_default();
        a.weight = a.weight.saturating_add(e.weight);
        if e.unix_ts >= recent_cutoff {
            a.recent_weight = a.recent_weight.saturating_add(e.weight);
        }
        if e.unix_ts >= hour_cutoff {
            a.shares_1h += 1;
        }
        a.last_share_time = Some(a.last_share_time.map_or(e.unix_ts, |t| t.max(e.unix_ts)));
    }
    for (spk, una) in s.blocks.iter().flat_map(|b| b.outputs.iter()) {
        if let Ok(script) = hex::decode(spk) {
            let a = per_script.entry(script).or_default();
            a.paid_una = a.paid_una.saturating_add(*una);
        }
    }
    Rendered {
        stats_json: encode(&compute_stats(s)),
        blocks: recent_blocks(s, MAX_BLOCKS_LIMIT),
        per_script,
        total_weight,
        span_secs: hashrate_span(now, s.window.iter()),
        fee_bps: s.fee_bps,
        next_reward_una: s.next_reward_una,
    }
}

/// `None` when the address has never contributed: not in the window and
/// never paid. That is what makes the route non-enumerable — a 404 says
/// "not a miner here", nothing more.
pub fn miner_stats(r: &Rendered, script: &[u8], address: &str) -> Option<MinerStats> {
    let agg = r.per_script.get(script)?;
    let window_bps = if r.total_weight == 0 {
        0
    } else {
        (agg.weight.saturating_mul(10_000) / r.total_weight) as u32
    };
    let after_fee = u128::from(r.next_reward_una)
        * u128::from(10_000u32.saturating_sub(r.fee_bps.min(10_000)))
        / 10_000;
    let est_next_block_una =
        u64::try_from(after_fee * u128::from(window_bps) / 10_000).unwrap_or(u64::MAX);
    Some(MinerStats {
        address: address.to_string(),
        hashrate_hs: u64::try_from(agg.recent_weight / u128::from(r.span_secs.max(1)))
            .unwrap_or(u64::MAX),
        shares_1h: agg.shares_1h,
        window_bps,
        est_next_block_una,
        paid_una: agg.paid_una,
        last_share_time: agg.last_share_time,
    })
}

/// `?limit=N` for `/api/blocks`: clamped to `1..=MAX_BLOCKS_LIMIT`, and
/// anything unparseable falls back to the default rather than erroring —
/// a public read endpoint should answer, not lecture.
pub fn parse_limit(query: &str) -> usize {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix("limit="))
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_BLOCKS_LIMIT)
        .clamp(1, MAX_BLOCKS_LIMIT)
}

// ---------------------------------------------------------------------
// Address <-> payout script
// ---------------------------------------------------------------------

const HRP: &str = "din";

/// `din1p…` (bech32m, witness v1, 32-byte program) to the 34-byte P2TR
/// script the PPLNS window is keyed by. `None` for anything else — wrong
/// network, wrong version, shielded, typo.
pub fn address_to_script(addr: &str) -> Option<Vec<u8>> {
    let (hrp, version, program) = bech32::segwit::decode(addr.trim()).ok()?;
    if hrp.to_lowercase() != HRP || version != bech32::Fe32::P || program.len() != 32 {
        return None;
    }
    let mut script = Vec::with_capacity(34);
    script.extend_from_slice(&[0x51, 0x20]);
    script.extend_from_slice(&program);
    Some(script)
}

/// Inverse of `address_to_script`. `None` unless the script is exactly
/// `5120<32 bytes>`.
pub fn script_to_address(script: &[u8]) -> Option<String> {
    if script.len() != 34 || script[0] != 0x51 || script[1] != 0x20 {
        return None;
    }
    let hrp = bech32::Hrp::parse(HRP).ok()?;
    bech32::segwit::encode(hrp, bech32::Fe32::P, &script[2..]).ok()
}

// ---------------------------------------------------------------------
// Found-block log
// ---------------------------------------------------------------------

#[derive(Default)]
struct BlockLogState {
    recent: VecDeque<FoundBlock>,
    total: u64,
    path: Option<PathBuf>,
}

/// Blocks this pool found. In memory for serving, appended to a JSONL
/// file so `blocks_found_total` and the recent list survive a restart.
/// Writing the log must never get in the way of accepting a block, so a
/// failed append is a warning, not an error.
#[derive(Default)]
pub struct BlockLog(Mutex<BlockLogState>);

static BLOCKS: OnceLock<BlockLog> = OnceLock::new();

pub fn blocks() -> &'static BlockLog {
    BLOCKS.get_or_init(BlockLog::default)
}

impl BlockLog {
    /// Load history from `path` and append future blocks to it.
    pub fn attach(&self, path: &Path) -> Result<()> {
        let mut loaded = Vec::new();
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                for (i, line) in raw.lines().enumerate() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<FoundBlock>(line) {
                        Ok(b) => loaded.push(b),
                        Err(e) => {
                            warn!(line = i + 1, error = %e, "skipping unreadable found-blocks entry")
                        }
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
        let mut s = self.0.lock().expect("block log mutex");
        s.total = s.total.saturating_add(loaded.len() as u64);
        for b in loaded {
            s.recent.push_back(b);
        }
        while s.recent.len() > MAX_BLOCK_HISTORY {
            s.recent.pop_front();
        }
        s.path = Some(path.to_path_buf());
        info!(path = %path.display(), blocks = s.total, "found-blocks log attached");
        Ok(())
    }

    pub fn record(&self, block: FoundBlock) {
        let mut s = self.0.lock().expect("block log mutex");
        if let Some(path) = s.path.clone() {
            let appended = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .and_then(|mut f| {
                    let mut line = serde_json::to_string(&block).unwrap_or_default();
                    line.push('\n');
                    f.write_all(line.as_bytes())
                });
            if let Err(e) = appended {
                warn!(path = %path.display(), error = %e, "could not append to found-blocks log");
            }
        }
        s.total = s.total.saturating_add(1);
        s.recent.push_back(block);
        while s.recent.len() > MAX_BLOCK_HISTORY {
            s.recent.pop_front();
        }
    }

    /// `(recent oldest-first, total ever)`.
    pub fn snapshot(&self) -> (Vec<FoundBlock>, u64) {
        let s = self.0.lock().expect("block log mutex");
        (s.recent.iter().cloned().collect(), s.total)
    }
}

// ---------------------------------------------------------------------
// Rate limiting and caching
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// Requests one IP may make per `window_secs`.
    pub max_requests: u32,
    pub window_secs: u64,
}

impl Default for RateLimit {
    fn default() -> Self {
        Self {
            max_requests: 60,
            window_secs: 60,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub rate_limit: RateLimit,
    pub cache_ttl: Duration,
    /// Concurrent connections served; anything beyond is closed on
    /// accept without a response.
    pub max_connections: usize,
    /// Hard deadline for one connection, accept to close. Bounds a client
    /// that trickles bytes (slowloris) or never reads its response.
    pub connection_timeout: Duration,
}

pub const DEFAULT_MAX_CONNECTIONS: usize = 256;
pub const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

impl Default for Config {
    fn default() -> Self {
        Self {
            rate_limit: RateLimit::default(),
            cache_ttl: DEFAULT_CACHE_TTL,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            connection_timeout: DEFAULT_CONNECTION_TIMEOUT,
        }
    }
}

/// The address a request should be rate limited as.
///
/// Behind a local reverse proxy every peer is loopback, so the proxy's
/// `X-Forwarded-For` (first hop) or `X-Real-IP` is the client. From any
/// other peer those headers are attacker-controlled and ignored: a direct
/// client cannot choose its own bucket. Unparseable values fall back to
/// the peer.
pub fn client_ip(peer: IpAddr, head: &str) -> IpAddr {
    if !peer.is_loopback() {
        return peer;
    }
    let mut forwarded: Option<&str> = None;
    let mut real: Option<&str> = None;
    for line in head.split("\r\n").skip(1) {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        if name.eq_ignore_ascii_case("x-forwarded-for") && forwarded.is_none() {
            forwarded = value.split(',').next().map(str::trim);
        } else if name.eq_ignore_ascii_case("x-real-ip") && real.is_none() {
            real = Some(value.trim());
        }
    }
    forwarded
        .and_then(|v| v.parse().ok())
        .or_else(|| real.and_then(|v| v.parse().ok()))
        .unwrap_or(peer)
}

/// Hard cap on distinct limiter keys. Reaching it sweeps expired windows;
/// if the table is still full, it is cleared outright — a flood of fresh
/// sources briefly resets everyone's count rather than growing memory.
pub const LIMITER_MAX_KEYS: usize = 50_000;

/// IPv4 as-is; IPv6 collapsed to its /64, since one host commonly holds
/// a whole /64 and could otherwise mint a fresh key per request.
fn bucket_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(_) => ip,
        IpAddr::V6(v6) => {
            let mut o = v6.octets();
            o[8..].fill(0);
            IpAddr::V6(o.into())
        }
    }
}

/// Fixed-window counter per client key. Bounded by `LIMITER_MAX_KEYS`.
pub struct Limiter {
    limit: RateLimit,
    buckets: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl Limiter {
    pub fn new(limit: RateLimit) -> Self {
        Self {
            limit,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn allow(&self, ip: IpAddr, now: Instant) -> bool {
        let key = bucket_key(ip);
        let window = Duration::from_secs(self.limit.window_secs.max(1));
        let mut b = self.buckets.lock().expect("limiter mutex");
        if b.len() >= LIMITER_MAX_KEYS && !b.contains_key(&key) {
            // One O(n) sweep per LIMITER_MAX_KEYS insertions at worst, and
            // never more than that many entries.
            b.retain(|_, (start, _)| now.duration_since(*start) < window);
            if b.len() >= LIMITER_MAX_KEYS {
                b.clear();
            }
        }
        let entry = b.entry(key).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        if entry.1 >= self.limit.max_requests {
            return false;
        }
        entry.1 += 1;
        true
    }

    pub fn len(&self) -> usize {
        self.buckets.lock().expect("limiter mutex").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct Cache<F> {
    source: Arc<F>,
    ttl: Duration,
    slot: Mutex<Option<(Instant, Arc<Rendered>)>>,
}

impl<F: Fn() -> Sample> Cache<F> {
    /// Double-checked: the lock is held only to read or to store, never
    /// while the sample is taken (which takes the PPLNS window lock) or
    /// rendered. Two requests racing across an expiry may both build; the
    /// first to store wins and the other adopts it.
    fn get(&self) -> Arc<Rendered> {
        if let Some((built, r)) = self.slot.lock().expect("stats cache mutex").as_ref() {
            if built.elapsed() < self.ttl {
                return r.clone();
            }
        }
        let fresh = Arc::new(render(&(self.source)()));
        let mut slot = self.slot.lock().expect("stats cache mutex");
        match slot.as_ref() {
            Some((built, r)) if built.elapsed() < self.ttl => r.clone(),
            _ => {
                *slot = Some((Instant::now(), fresh.clone()));
                fresh
            }
        }
    }
}

// ---------------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------------

/// `(method, path, query)` from a request head. `None` when malformed.
fn parse_request_line(head: &str) -> Option<(String, String, String)> {
    let line = head.split("\r\n").next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?;
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    Some((method, path, query))
}

struct Response {
    status: &'static str,
    content_type: &'static str,
    body: String,
    extra: Vec<(&'static str, String)>,
}

impl Response {
    fn json(status: &'static str, body: String) -> Self {
        Self {
            status,
            content_type: "application/json",
            body,
            extra: Vec::new(),
        }
    }
    fn error(status: &'static str, message: &str) -> Self {
        Self::json(status, serde_json::json!({ "error": message }).to_string())
    }
}

const OK: &str = "200 OK";
const BAD_REQUEST: &str = "400 Bad Request";
const NOT_FOUND: &str = "404 Not Found";
const METHOD_NOT_ALLOWED: &str = "405 Method Not Allowed";
const TOO_MANY: &str = "429 Too Many Requests";

/// The router. Knows exactly four things and nothing about ops.
///
/// Path is matched BEFORE method: an ops path (or any other unknown path)
/// is "not here" whatever the verb, and 405 is reserved for a path this
/// listener actually serves. Answering 405 first would confirm to a
/// prober that `/withdraw` exists somewhere.
fn route(method: &str, path: &str, query: &str, r: &Rendered) -> Response {
    let known = matches!(path, "/" | "/index.html" | STATS_PATH | BLOCKS_PATH)
        || path.starts_with(MINER_PREFIX);
    if !known {
        return Response::error(NOT_FOUND, "not found");
    }
    if method != "GET" && method != "HEAD" {
        let mut r = Response::error(METHOD_NOT_ALLOWED, "read-only: GET or HEAD");
        r.extra.push(("Allow", "GET, HEAD".to_string()));
        return r;
    }
    match path {
        "/" | "/index.html" => Response {
            status: OK,
            content_type: "text/html; charset=utf-8",
            body: INDEX_HTML.to_string(),
            extra: Vec::new(),
        },
        STATS_PATH => Response::json(OK, r.stats_json.clone()),
        BLOCKS_PATH => {
            let n = parse_limit(query).min(r.blocks.len());
            Response::json(OK, encode(&r.blocks[..n]))
        }
        p if p.starts_with(MINER_PREFIX) => {
            let addr = &p[MINER_PREFIX.len()..];
            let Some(script) = address_to_script(addr) else {
                return Response::error(BAD_REQUEST, "expected a din1p... taproot address");
            };
            match miner_stats(r, &script, &addr.trim().to_lowercase()) {
                Some(m) => Response::json(OK, encode(&m)),
                None => Response::error(NOT_FOUND, "not found"),
            }
        }
        _ => Response::error(NOT_FOUND, "not found"),
    }
}

fn encode<T: Serialize + ?Sized>(v: &T) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "{\"error\":\"encode failed\"}".to_string())
}

async fn write_response(
    sock: &mut TcpStream,
    r: &Response,
    head_only: bool,
    cors: bool,
    ttl: Duration,
) {
    let mut head = format!(
        "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         Cache-Control: public, max-age={}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        r.status,
        r.content_type,
        r.body.len(),
        ttl.as_secs(),
    );
    if cors {
        head.push_str("Access-Control-Allow-Origin: *\r\n");
    }
    for (name, value) in &r.extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = sock.write_all(head.as_bytes()).await;
    if !head_only {
        let _ = sock.write_all(r.body.as_bytes()).await;
    }
    let _ = sock.shutdown().await;
}

/// Serve the public stats listener until it dies. `sample` is called at
/// most once per `config.cache_ttl`, however many requests arrive.
pub async fn serve<F>(listener: TcpListener, sample: Arc<F>, config: Config) -> Result<()>
where
    F: Fn() -> Sample + Send + Sync + 'static,
{
    let limiter = Arc::new(Limiter::new(config.rate_limit));
    let cache = Arc::new(Cache {
        source: sample,
        ttl: config.cache_ttl,
        slot: Mutex::new(None),
    });
    let permits = Arc::new(Semaphore::new(config.max_connections.max(1)));
    let deadline = config.connection_timeout;
    loop {
        let (sock, peer) = match listener.accept().await {
            Ok(v) => v,
            Err(e) => {
                // EMFILE and friends: back off instead of spinning the core.
                warn!(error = %e, "public stats accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Over the cap: close on accept, no task, no response.
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            drop(sock);
            continue;
        };
        let limiter = limiter.clone();
        let cache = cache.clone();
        tokio::spawn(async move {
            let _permit = permit;
            // The whole conversation — head read, render, write — is under
            // one deadline. On expiry the future is dropped and the socket
            // closes with it.
            let _ = tokio::time::timeout(deadline, handle(sock, peer.ip(), limiter, cache)).await;
        });
    }
}

async fn handle<F: Fn() -> Sample>(
    mut sock: TcpStream,
    peer: IpAddr,
    limiter: Arc<Limiter>,
    cache: Arc<Cache<F>>,
) {
    let ttl = cache.ttl;
    let Some(raw) = crate::ops::read_head(&mut sock).await else {
        let r = Response::error(BAD_REQUEST, "bad request");
        write_response(&mut sock, &r, false, false, ttl).await;
        return;
    };
    let Some((head, _body)) = crate::ops::split_head_body(&raw) else {
        let r = Response::error(BAD_REQUEST, "bad request");
        write_response(&mut sock, &r, false, false, ttl).await;
        return;
    };
    let Some((method, path, query)) = parse_request_line(&head) else {
        let r = Response::error(BAD_REQUEST, "bad request");
        write_response(&mut sock, &r, false, false, ttl).await;
        return;
    };
    let head_only = method == "HEAD";
    let cors = method == "GET" || head_only;
    // Keyed after the head is read (the forwarded address lives in it)
    // but before any work: an over-limit client costs a bounded read and
    // a one-line answer, never a render.
    if !limiter.allow(client_ip(peer, &head), Instant::now()) {
        let mut r = Response::error(TOO_MANY, "rate limited");
        r.extra
            .push(("Retry-After", limiter.limit.window_secs.to_string()));
        write_response(&mut sock, &r, head_only, cors, ttl).await;
        return;
    }
    let rendered = cache.get();
    let r = route(&method, &path, &query, &rendered);
    write_response(&mut sock, &r, head_only, cors, ttl).await;
}

/// Bind the public listener. Unlike the ops listener this one is MEANT to
/// be reachable, so an off-loopback bind is logged as information, not a
/// warning; TLS is still a reverse proxy's job.
pub async fn bind(addr: &str) -> Result<TcpListener> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding public stats endpoint on {addr}"))?;
    info!(%addr, "public stats endpoint listening (unauthenticated, read-only)");
    Ok(listener)
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ADDR: &str = "din1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggxy";
    const SCRIPT_HEX: &str = "5120499c2aee9ac29d750af0ad6a752ab3c549b8c1862b3bc31a0091d9715a676bd8";

    fn entry(script: &[u8], weight: u128, ts: u64) -> WindowEntry {
        WindowEntry {
            payout_script: script.to_vec(),
            weight,
            unix_ts: ts,
        }
    }

    // ---- address conversion (vectors from an independent bech32m impl) ----

    #[test]
    fn address_round_trips_through_the_payout_script() {
        let script = address_to_script(ADDR).expect("decodes");
        assert_eq!(hex::encode(&script), SCRIPT_HEX);
        assert_eq!(script_to_address(&script).as_deref(), Some(ADDR));
        assert_eq!(
            script_to_address(&hex::decode(format!("5120{}", "ab".repeat(32))).unwrap()).as_deref(),
            Some("din1p4w46h2at4w46h2at4w46h2at4w46h2at4w46h2at4w46h2at4w4sy4ndpg")
        );
    }

    #[test]
    fn address_decoding_accepts_uppercase_and_whitespace() {
        assert_eq!(
            address_to_script(&format!("  {}  ", ADDR.to_uppercase())),
            address_to_script(ADDR)
        );
    }

    #[test]
    fn non_taproot_or_foreign_addresses_are_refused() {
        for bad in [
            "",
            "din1p",
            "not-an-address",
            "bc1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggxy",
            "din1pfxwz4m56c2wh2zhs4448224nc4ym3svx9vauxxsqj8vhzkn8d0vq92ggxz", // checksum
        ] {
            assert!(address_to_script(bad).is_none(), "{bad:?}");
        }
        // v0 on the din hrp is not a payout target either.
        let hrp = bech32::Hrp::parse("din").unwrap();
        let v0 = bech32::segwit::encode(hrp, bech32::Fe32::Q, &[0u8; 20]).unwrap();
        assert!(address_to_script(&v0).is_none());
        assert!(script_to_address(&[0x51, 0x20, 0, 0]).is_none());
        assert!(script_to_address(&[0u8; 34]).is_none());
    }

    // ---- difficulty ----

    #[test]
    fn difficulty_matches_the_bitcoin_convention() {
        assert_eq!(difficulty_from_nbits(0x1d00ffff), 1.0);
        let d = difficulty_from_nbits(0x1b0404cb);
        assert!((d - 16_307.420_938).abs() < 0.001, "{d}");
        assert_eq!(difficulty_from_nbits(0x1d000000), 0.0);
    }

    // ---- hashrate / stats ----

    #[test]
    fn hashrate_is_weight_over_the_window_and_shortens_for_a_young_pool() {
        let s = hex::decode(SCRIPT_HEX).unwrap();
        let now = 10_000;
        // Pool started 100s ago: 5 shares of 200 → 1000 hashes / 100s.
        let young: Vec<_> = (0..5).map(|i| entry(&s, 200, now - 100 + i * 20)).collect();
        assert_eq!(hashrate_hs(now, young.iter(), young.iter()), 10);
        // Old pool: only the last 600s count, over the full 600s span.
        let mut old = vec![entry(&s, 1_000_000, now - 5_000)];
        old.extend((0..6).map(|i| entry(&s, 600, now - 600 + i * 100)));
        assert_eq!(hashrate_hs(now, old.iter(), old.iter()), 6);
        assert_eq!(hashrate_hs(now, std::iter::empty(), std::iter::empty()), 0);
    }

    #[test]
    fn stats_count_only_recently_active_miners_and_24h_blocks() {
        let a = hex::decode(SCRIPT_HEX).unwrap();
        let mut b = a.clone();
        b[5] ^= 1;
        let mut c = a.clone();
        c[6] ^= 1;
        let now = 1_000_000;
        let sample = Sample {
            now_unix: now,
            window: vec![
                entry(&a, 1, now - 10),
                entry(&a, 1, now - 20),
                entry(&b, 1, now - ACTIVE_MINER_SECS), // boundary: active
                entry(&c, 1, now - ACTIVE_MINER_SECS - 1), // just too old
            ],
            blocks: vec![
                FoundBlock {
                    height: 1,
                    hash: "a".into(),
                    time: now - DAY_SECS - 1,
                    reward_una: 1,
                    status: "accepted".into(),
                    outputs: vec![],
                },
                FoundBlock {
                    height: 2,
                    hash: "b".into(),
                    time: now - DAY_SECS,
                    reward_una: 1,
                    status: "accepted".into(),
                    outputs: vec![],
                },
                FoundBlock {
                    height: 3,
                    hash: "c".into(),
                    time: now - 1,
                    reward_una: 1,
                    status: "accepted".into(),
                    outputs: vec![],
                },
            ],
            blocks_found_total: 30,
            fee_bps: 200,
            connected_workers: 9,
            ..Sample::default()
        };
        let st = compute_stats(&sample);
        assert_eq!(st.miners, 2);
        assert_eq!(st.workers, 9);
        assert_eq!(st.blocks_found_24h, 2);
        assert_eq!(st.blocks_found_total, 30);
        assert_eq!(st.last_block_height, Some(3));
        assert_eq!(st.last_block_hash.as_deref(), Some("c"));
        assert_eq!(st.payout_scheme, "PPLNS");
        assert_eq!(st.updated_at, now);
    }

    #[test]
    fn empty_pool_reports_zeros_not_errors() {
        let st = compute_stats(&Sample::default());
        assert_eq!(st.pool_hashrate_hs, 0);
        assert_eq!(st.miners, 0);
        assert_eq!(st.last_block_height, None);
        assert!(recent_blocks(&Sample::default(), 10).is_empty());
    }

    // ---- blocks ----

    #[test]
    fn limit_is_clamped_and_defaults_on_junk() {
        assert_eq!(parse_limit(""), DEFAULT_BLOCKS_LIMIT);
        assert_eq!(parse_limit("limit=7"), 7);
        assert_eq!(parse_limit("a=1&limit=7&b=2"), 7);
        assert_eq!(parse_limit("limit=0"), 1);
        assert_eq!(parse_limit("limit=-3"), DEFAULT_BLOCKS_LIMIT);
        assert_eq!(parse_limit("limit=abc"), DEFAULT_BLOCKS_LIMIT);
        assert_eq!(parse_limit("limit=1000"), MAX_BLOCKS_LIMIT);
        assert_eq!(
            parse_limit("limit=99999999999999999999999"),
            DEFAULT_BLOCKS_LIMIT
        );
    }

    #[test]
    fn recent_blocks_are_newest_first_without_outputs() {
        let sample = Sample {
            blocks: (1..=3)
                .map(|h| FoundBlock {
                    height: h,
                    hash: h.to_string(),
                    time: h,
                    reward_una: 5,
                    status: "accepted".into(),
                    outputs: vec![("5120aa".into(), 5)],
                })
                .collect(),
            ..Sample::default()
        };
        let got = recent_blocks(&sample, 2);
        assert_eq!(got.iter().map(|b| b.height).collect::<Vec<_>>(), vec![3, 2]);
        assert!(!encode(&got).contains("outputs"));
    }

    // ---- miner ----

    #[test]
    fn miner_stats_are_none_for_a_stranger_and_estimate_pending_for_a_contributor() {
        let a = hex::decode(SCRIPT_HEX).unwrap();
        let mut b = a.clone();
        b[5] ^= 1;
        let now = 1_000_000;
        let sample = Sample {
            now_unix: now,
            window: vec![entry(&a, 300, now - 30), entry(&b, 100, now - 30)],
            blocks: vec![FoundBlock {
                height: 1,
                hash: "a".into(),
                time: now - 5,
                reward_una: 1000,
                status: "accepted".into(),
                outputs: vec![(SCRIPT_HEX.into(), 700), ("5120bb".into(), 300)],
            }],
            fee_bps: 1_000,
            next_reward_una: 1_000,
            ..Sample::default()
        };
        let mut stranger = a.clone();
        stranger[7] ^= 1;
        let r = render(&sample);
        assert_eq!(miner_stats(&r, &stranger, "x"), None);

        let m = miner_stats(&r, &a, ADDR).unwrap();
        assert_eq!(m.address, ADDR);
        assert_eq!(m.window_bps, 7_500);
        // 1000 × 0.9 × 0.75
        assert_eq!(m.est_next_block_una, 675);
        assert_eq!(m.paid_una, 700);
        assert_eq!(m.shares_1h, 1);
        assert_eq!(m.last_share_time, Some(now - 30));
        // 300 hashes over a 30s-old pool.
        assert_eq!(m.hashrate_hs, 10);
    }

    #[test]
    fn a_paid_miner_that_left_the_window_is_still_known() {
        let a = hex::decode(SCRIPT_HEX).unwrap();
        let sample = Sample {
            now_unix: 100,
            blocks: vec![FoundBlock {
                height: 1,
                hash: "a".into(),
                time: 1,
                reward_una: 9,
                status: "accepted".into(),
                outputs: vec![(SCRIPT_HEX.into(), 9)],
            }],
            ..Sample::default()
        };
        let m = miner_stats(&render(&sample), &a, ADDR).unwrap();
        assert_eq!(m.paid_una, 9);
        assert_eq!(m.window_bps, 0);
        assert_eq!(m.last_share_time, None);
    }

    // ---- router: the isolation contract, at the unit level ----

    #[test]
    fn router_knows_no_ops_route_under_any_method() {
        let s = render(&Sample::default());
        for path in [
            "/status",
            "/payout-address",
            "/fee-bps",
            "/ban",
            "/unban",
            "/withdraw",
        ] {
            for method in ["GET", "HEAD", "POST", "PUT", "DELETE"] {
                assert_eq!(
                    route(method, path, "", &s).status,
                    NOT_FOUND,
                    "{method} {path}"
                );
            }
        }
        assert_eq!(route("POST", STATS_PATH, "", &s).status, METHOD_NOT_ALLOWED);
        assert_eq!(route("GET", "/api/miner", "", &s).status, NOT_FOUND);
        assert_eq!(route("GET", "/api/miners", "", &s).status, NOT_FOUND);
        assert_eq!(route("GET", "/api/miner/junk", "", &s).status, BAD_REQUEST);
        assert_eq!(
            route("GET", &format!("/api/miner/{ADDR}"), "", &s).status,
            NOT_FOUND
        );
        assert_eq!(route("GET", STATS_PATH, "", &s).status, OK);
        assert_eq!(
            route("GET", "/", "", &s).content_type,
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn request_line_keeps_the_query() {
        assert_eq!(
            parse_request_line("GET /api/blocks?limit=3 HTTP/1.1\r\nHost: x\r\n\r\n"),
            Some(("GET".into(), "/api/blocks".into(), "limit=3".into()))
        );
        assert_eq!(parse_request_line("GET\r\n\r\n"), None);
        assert_eq!(parse_request_line(""), None);
    }

    // ---- limiter ----

    #[test]
    fn limiter_allows_up_to_max_then_refuses_until_the_window_turns() {
        let l = Limiter::new(RateLimit {
            max_requests: 2,
            window_secs: 10,
        });
        let ip: IpAddr = "10.0.0.1".parse().unwrap();
        let other: IpAddr = "10.0.0.2".parse().unwrap();
        let t0 = Instant::now();
        assert!(l.allow(ip, t0));
        assert!(l.allow(ip, t0 + Duration::from_secs(1)));
        assert!(!l.allow(ip, t0 + Duration::from_secs(2)));
        // Another IP is unaffected.
        assert!(l.allow(other, t0 + Duration::from_secs(2)));
        // Window turns over.
        assert!(l.allow(ip, t0 + Duration::from_secs(10)));
    }

    // ---- block log ----

    #[test]
    fn block_log_persists_and_reloads_with_a_total_beyond_the_ring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLOCK_LOG_BASENAME);
        let log = BlockLog::default();
        log.attach(&path).unwrap();
        for h in 0..(MAX_BLOCK_HISTORY as u64 + 5) {
            log.record(FoundBlock {
                height: h,
                hash: h.to_string(),
                time: h,
                reward_una: 1,
                status: "accepted".into(),
                outputs: vec![],
            });
        }
        let (recent, total) = log.snapshot();
        assert_eq!(total, MAX_BLOCK_HISTORY as u64 + 5);
        assert_eq!(recent.len(), MAX_BLOCK_HISTORY);
        assert_eq!(recent.last().unwrap().height, MAX_BLOCK_HISTORY as u64 + 4);

        let reloaded = BlockLog::default();
        reloaded.attach(&path).unwrap();
        let (recent2, total2) = reloaded.snapshot();
        assert_eq!(total2, total);
        assert_eq!(recent2, recent);
    }

    #[test]
    fn block_log_tolerates_a_missing_file_and_a_corrupt_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(BLOCK_LOG_BASENAME);
        let log = BlockLog::default();
        log.attach(&path).unwrap();
        assert_eq!(log.snapshot().1, 0);
        std::fs::write(&path, "{\"height\":1,\"hash\":\"a\",\"time\":1,\"reward_una\":1,\"status\":\"accepted\"}\nnot json\n").unwrap();
        let log2 = BlockLog::default();
        log2.attach(&path).unwrap();
        assert_eq!(log2.snapshot().1, 1);
    }

    // ---- client keying behind a proxy ----

    #[test]
    fn forwarded_address_is_honoured_only_from_a_loopback_peer() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let lo6: IpAddr = "::1".parse().unwrap();
        let remote: IpAddr = "198.51.100.7".parse().unwrap();
        let xff = "GET / HTTP/1.1\r\nX-Forwarded-For: 203.0.113.9, 10.0.0.1\r\n\r\n";
        let real = "GET / HTTP/1.1\r\nx-real-ip: 203.0.113.8\r\n\r\n";
        let none = "GET / HTTP/1.1\r\nHost: x\r\n\r\n";
        let junk = "GET / HTTP/1.1\r\nX-Forwarded-For: not-an-ip\r\n\r\n";
        assert_eq!(client_ip(lo, xff), "203.0.113.9".parse::<IpAddr>().unwrap());
        assert_eq!(
            client_ip(lo6, xff),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            client_ip(lo, real),
            "203.0.113.8".parse::<IpAddr>().unwrap()
        );
        assert_eq!(client_ip(lo, none), lo);
        assert_eq!(client_ip(lo, junk), lo);
        // A direct client cannot pick its own bucket.
        assert_eq!(client_ip(remote, xff), remote);
        assert_eq!(client_ip(remote, real), remote);
    }

    // ---- limiter bounds ----

    #[test]
    fn limiter_never_holds_more_than_the_cap() {
        let l = Limiter::new(RateLimit {
            max_requests: 10,
            window_secs: 60,
        });
        let t0 = Instant::now();
        for i in 0..(LIMITER_MAX_KEYS as u32 + 1_000) {
            let ip = IpAddr::from(std::net::Ipv4Addr::from(0x0a00_0000u32 + i));
            assert!(l.allow(ip, t0));
        }
        assert!(l.len() <= LIMITER_MAX_KEYS, "{}", l.len());
    }

    #[test]
    fn ipv6_clients_share_a_bucket_per_64() {
        let l = Limiter::new(RateLimit {
            max_requests: 1,
            window_secs: 60,
        });
        let t0 = Instant::now();
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert!(l.allow(a, t0));
        assert!(!l.allow(b, t0), "same /64 must share the bucket");
        assert!(l.allow(c, t0), "a different /64 is a different client");
    }

    // ---- rendered sample ----

    #[test]
    fn render_precomputes_stats_json_and_per_script_aggregates() {
        let a = hex::decode(SCRIPT_HEX).unwrap();
        let now = 1_000_000;
        let sample = Sample {
            now_unix: now,
            window: vec![entry(&a, 5, now - 1), entry(&a, 7, now - HOUR_SECS - 1)],
            fee_bps: 0,
            next_reward_una: 100,
            ..Sample::default()
        };
        let r = render(&sample);
        let v: serde_json::Value = serde_json::from_str(&r.stats_json).unwrap();
        assert_eq!(v["updated_at"], now);
        let m = miner_stats(&r, &a, ADDR).unwrap();
        assert_eq!(m.shares_1h, 1);
        assert_eq!(m.window_bps, 10_000);
        assert_eq!(m.est_next_block_una, 100);
        assert_eq!(m.last_share_time, Some(now - 1));
    }
}
