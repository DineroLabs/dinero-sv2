//! `dinero-sv2-pool` — Phase 4 reference pool server.
//!
//! Extends the template-provider flow from `dinero-tp` with two-tier
//! target acceptance and `submitblock` on found blocks:
//!
//! - Any share whose header hash ≤ `--share-leading-bits` (a loose
//!   pool-local target) gets credited to the miner.
//! - Any share whose header hash ≤ `block_target` (from
//!   `template.difficulty`) causes the pool to assemble the full block
//!   and `submitblock` it to dinerod; result logged.
//!
//! Phase 4 explicitly keeps pool-built coinbase, empty mempools, and
//! no persistent share ledger. See crate docs in
//! `~/.claude/plans/lovely-chasing-puzzle.md` for the longer roadmap.

use dinero_sv2_pool::{
    accounting, backend, block, dedup, job_generation, journal, mapper, ops, rpc, shared_template,
    split, supervisor, target,
};

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::Parser;
use dinero_sv2_codec::{
    decode_open_standard_mining_channel, decode_setup_connection, decode_submit_shares,
    decode_submit_shares_extended, encode_coinbase_context, encode_new_template,
    encode_open_standard_mining_channel_error, encode_open_standard_mining_channel_success,
    encode_set_new_prev_hash, encode_set_target, encode_setup_connection_error,
    encode_setup_connection_success, encode_submit_shares_error, encode_submit_shares_success,
    sv2::{decode_set_reward_mode, encode_window_status},
};
use dinero_sv2_common::{
    CoinbaseContext, HeaderAssembly, NewTemplateDinero, OpenStandardMiningChannelError,
    OpenStandardMiningChannelSuccess, SetNewPrevHash, SetupConnectionError, SetupConnectionSuccess,
    SubmitSharesDinero, SubmitSharesError, SubmitSharesSuccess, WindowStatus, PROTOCOL_MINING,
    PROTOCOL_VERSION,
};
use dinero_sv2_jd::{
    assemble_stripped_coinbase, commitment as utreexo_commitment, compute_root,
    encode_utreexo_accumulator_state,
    filter_commitment::{is_dnrf_script, requires_filter_commitment},
    leaf_hash_for_height,
    witness_commitment::{
        build_dnrw_script, is_dnrw_script, requires_witness_commitment, witness_merkle_root,
        wtxid_from_tx_bytes,
    },
    CoinbaseOutput, UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET,
};
use dinero_sv2_transport::{
    Frame, NoiseSession, StaticKeys, MSG_COINBASE_CONTEXT, MSG_NEW_MINING_JOB,
    MSG_OPEN_STANDARD_MINING_CHANNEL, MSG_OPEN_STANDARD_MINING_CHANNEL_ERROR,
    MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS, MSG_SETUP_CONNECTION, MSG_SETUP_CONNECTION_ERROR,
    MSG_SETUP_CONNECTION_SUCCESS, MSG_SET_NEW_PREV_HASH, MSG_SET_REWARD_MODE, MSG_SET_TARGET,
    MSG_SUBMIT_SHARES_ERROR, MSG_SUBMIT_SHARES_EXTENDED, MSG_SUBMIT_SHARES_STANDARD,
    MSG_SUBMIT_SHARES_SUCCESS, MSG_UTREEXO_STATE, MSG_WINDOW_STATUS,
};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use crate::accounting::{share_weight, Ledger, MinerKey, PplnsWindow, WindowEntry};
use crate::backend::BackendPool;
use crate::dedup::ShareDedup;
use crate::journal::WindowJournal;
use crate::mapper::PoolTemplate;
use crate::rpc::{Auth, RpcClient, SubmitBlockResult};
use crate::shared_template::SharedTemplate;
use crate::target::{hash_meets_target, leading_zero_bits_target, target_for_hashrate};
use dinero_sv2_pool::{fee, payout};

/// Bundle of the daemon-sourced solo template and (if it could be
/// built this refresh) the pool-owned shared-mode variant. Sent as a
/// single atomic unit over the watch channel so a connection never
/// observes a `shared` template paired with a stale/mismatched `pt`
/// (or vice versa) — see amendments for Task 6.
struct TemplateBundle {
    pt: PoolTemplate,
    /// PPLNS split outputs for this refresh (value outputs only), from
    /// which each shared channel derives its OWN template with a
    /// per-channel scriptSig extranonce — so no two channels ever grind
    /// the same header. `None` when the shared build can't work this
    /// refresh (pre-flighted in the producer).
    shared_split: Option<Vec<CoinbaseOutput>>,
    /// Whether the *solo* template materially changed this refresh
    /// (new tip or new nbits). `false` on a bundle republished solely
    /// because `--refresh-same-tip-secs` elapsed (a `stale_same_tip`
    /// tick) — those exist to keep the SHARED template's PPLNS weights
    /// and curtime fresh, not to reissue identical solo work. See the
    /// producer loop and `serve_miner`'s `rx.changed` arm.
    solo_changed: bool,
    /// Monotonic backend-selection generation. A change forces a fresh job and
    /// makes every prior job_id stale even when both healthy daemons share the
    /// same chain tip.
    backend_epoch: u64,
    backend_endpoint: String,
}

/// Hash a shared-mode miner's payout script into the ledger's
/// fixed-size `MinerKey`, so the existing `[u8; 32]`-keyed `Ledger`
/// works for shared miners without a refactor (amendment 6).
fn miner_key_for_payout_script(payout_script: &[u8]) -> MinerKey {
    let mut hasher = Sha256::new();
    hasher.update(payout_script);
    hasher.finalize().into()
}

/// Per-channel vardiff config. `None` = vardiff off (use fallback target
/// from `--share-leading-bits` for everyone, legacy behaviour).
#[derive(Debug, Clone, Copy)]
struct VardiffConfig {
    /// Target ~1 share per N seconds per channel.
    target_interval_secs: f64,
    /// Recompute observed-rate-based target every N seconds. `None`
    /// means "initial target only, no follow-up retargeting".
    window: Option<Duration>,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Bind address for miner connections.
    #[arg(long, default_value = "127.0.0.1:4444")]
    bind: SocketAddr,

    /// dinerod RPC URL. Repeat this option (or use comma-separated values) to
    /// configure health-checked failover backends.
    #[arg(long, value_delimiter = ',', default_value = "http://127.0.0.1:20998")]
    rpc_url: Vec<String>,

    /// Cookie file (ignored if --rpc-user / --rpc-password are set).
    #[arg(long)]
    cookie: Option<String>,

    /// Explicit rpcuser.
    #[arg(long)]
    rpc_user: Option<String>,

    /// Explicit rpcpassword.
    #[arg(long)]
    rpc_password: Option<String>,

    /// Payout address for getblocktemplate (pool-built coinbase; miners
    /// don't alter outputs in Phase 4).
    ///
    /// This is the INITIAL value. If `--payout-address-file` holds a
    /// plausible address, that wins — it is what the operator last chose at
    /// runtime, and silently reverting it on restart would pay the wrong
    /// destination for however long nobody noticed.
    ///
    /// Required to RUN the pool, but not to `--print-pubkey`: making clap
    /// demand it there meant the installer's bare `--print-pubkey` exited 2
    /// with empty stdout, so operators were never shown the key their miners
    /// must pin.
    #[arg(long, required_unless_present = "print_pubkey")]
    payout_address: Option<String>,

    /// Where a runtime-set payout address is persisted, so it survives a
    /// restart. Read at startup in preference to `--payout-address`.
    #[arg(long, default_value = dinero_sv2_pool::payout::DEFAULT_PATH)]
    payout_address_file: PathBuf,

    /// Allow `POST /payout-address` on the ops endpoint, letting an operator
    /// retarget their own fee output from a client such as dinero-qt.
    ///
    /// OFF by default, and deliberately so: turning it on upgrades the ops
    /// token from a read credential into one that can redirect the operator's
    /// share of every future block. It cannot touch miners' payouts — those
    /// come from the PPLNS window, which no ops route reaches.
    #[arg(long, default_value_t = false)]
    ops_allow_payout_change: bool,

    /// Where a runtime-set operator fee is persisted across restarts.
    #[arg(long, default_value = dinero_sv2_pool::fee::DEFAULT_PATH)]
    shared_fee_bps_file: PathBuf,

    /// Allow authenticated `POST /fee-bps` runtime fee changes. OFF by default.
    #[arg(long, default_value_t = false)]
    ops_allow_fee_change: bool,

    /// Tip-poll interval.
    #[arg(long, default_value_t = 2)]
    poll_secs: u64,

    /// Kill the process if the template producer stops checking in for
    /// this many seconds, so systemd restarts it (the unit is
    /// `Restart=on-failure`). Guards the WEDGE case — a task that is
    /// still alive but stuck, which the producer-exit path cannot see.
    /// Deliberately far above worst-case iteration time: the RPC client
    /// times out at 15s per request, so even several failing calls in a
    /// row stay well under this. 0 disables.
    #[arg(long, default_value_t = supervisor::DEFAULT_STALL_SECS)]
    template_stall_secs: u64,

    /// Force-refresh the in-flight template at most this often, even
    /// when the chain tip hasn't changed. Picks up ASERT difficulty
    /// drift while the chain stalls (the daemon's getblocktemplate
    /// returns easier nBits as the proposed-ntime advances). Without
    /// this, miners are stuck mining against the stale (harder) target
    /// from the last prev_hash change. Set to 0 to disable.
    #[arg(long, default_value_t = 15)]
    refresh_same_tip_secs: u64,

    /// Fallback share-acceptance target as leading zero bits. Used as
    /// the channel's target ONLY when vardiff can't infer a real
    /// hashrate (miner reports 0 in OpenStandardMiningChannel and never
    /// produces a share). With vardiff active, each channel's effective
    /// target is sized off the miner's reported / observed hashrate.
    ///
    /// Capped at 96: `accounting::share_weight` only distinguishes
    /// difficulty for targets `>= 2^128` (fewer than 128 leading zero
    /// bits); staying at or below 96 keeps a wide safety margin so
    /// share weights never collapse into `u128::MAX` saturation.
    #[arg(long, default_value_t = 8, value_parser = clap::value_parser!(u32).range(0..=96))]
    share_leading_bits: u32,

    /// Vardiff target: aim for ~1 accepted share per N seconds per
    /// channel. Smaller = faster UI feedback, more share traffic; larger
    /// = sparser shares, less network/log noise. Set to 0 to disable
    /// vardiff and use `--share-leading-bits` for everyone (legacy).
    #[arg(long, default_value_t = 5)]
    vardiff_target_seconds: u64,

    /// Vardiff measurement window: recompute the per-channel target
    /// every N seconds based on observed share rate. The new target is
    /// emitted as `MSG_SET_TARGET` (0x22). Forward-compatible — clients
    /// that don't recognise the opcode keep mining at their channel-open
    /// target. Set to 0 to disable runtime adjustment (initial target
    /// from `nominal_hash_rate_bits` only, no follow-up).
    #[arg(long, default_value_t = 30)]
    vardiff_window_seconds: u64,

    /// Static Noise identity file.
    #[arg(long)]
    tp_key: Option<PathBuf>,

    /// Print the pool's static public key (hex) and exit.
    #[arg(long)]
    print_pubkey: bool,

    /// PPLNS operator fee in basis points (200 = 2%).
    #[arg(long, default_value_t = 200)]
    shared_fee_bps: u32,

    /// Max contributor outputs per shared block (fee output excluded).
    #[arg(long, default_value_t = 20)]
    shared_max_outputs: usize,

    /// Minimum contributor output value in una; smaller slices carry
    /// forward (stay credited in the window, just not paid this block).
    #[arg(long, default_value_t = 10_000)]
    shared_dust_una: u64,

    /// Read-only operator status endpoint (`GET /status`, bearer auth).
    /// Loopback by default: it speaks plain HTTP on purpose, so remote
    /// access goes through a TLS reverse proxy or an SSH tunnel rather
    /// than a TLS stack inside the pool binary. Empty disables it.
    #[arg(long, default_value = "127.0.0.1:4445")]
    ops_bind: String,

    /// File holding the ops bearer token. A file, not a flag, so the
    /// token never shows up in `ps` or the systemd unit.
    #[arg(long, default_value = "/etc/dinero-sv2/ops-token")]
    ops_token_file: PathBuf,

    /// PPLNS window journal path.
    #[arg(long, default_value = "/var/lib/dinero-sv2/pplns-journal.jsonl")]
    pplns_journal: PathBuf,

    /// Utreexo maturity-leaf hard-fork activation height for the
    /// network this pool's dinerod is running. Coinbase leaves created
    /// at/above this height hash with the v2 (maturity-bound) preimage;
    /// below it, v1. Mirrors dinerod's
    /// `UTREEXO_MATURITY_LEAF_HEIGHT_{MAINNET,TESTNET,REGTEST}`
    /// (mainnet=60_000 [default], testnet=0, regtest=20). Getting this
    /// wrong makes every pool-recomputed `utreexo_root` past the real
    /// activation height diverge from the daemon's, and `submitblock`
    /// rejects with `bad-utreexo-root`.
    #[arg(long, default_value_t = UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET)]
    utreexo_maturity_leaf_height: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let key_path = args.tp_key.clone().unwrap_or_else(default_pool_key_path);
    let static_keys = StaticKeys::load_or_generate(&key_path)
        .with_context(|| format!("loading pool key from {}", key_path.display()))?;

    if args.print_pubkey {
        println!("{}", static_keys.public_hex());
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dinero_sv2_pool=info".into()),
        )
        .init();
    info!(
        key = %key_path.display(),
        pubkey = %static_keys.public_hex(),
        "pool static identity"
    );

    let auth = match (&args.rpc_user, &args.rpc_password, &args.cookie) {
        (Some(u), Some(p), _) => Auth::UserPass(u.clone(), p.clone()),
        _ => Auth::Cookie(args.cookie.clone().unwrap_or_else(default_cookie_path)),
    };
    let rpc_clients = args
        .rpc_url
        .iter()
        .map(|url| RpcClient::new(url.clone(), auth.clone()).map(Arc::new))
        .collect::<Result<Vec<_>>>()
        .context("building rpc clients")?;
    let backends = Arc::new(BackendPool::new(rpc_clients)?);
    info!(
        backend_count = backends.len(),
        "configured dinerod backend pool"
    );

    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("binding {}", args.bind))?;
    info!(bind = %args.bind, "dinero-sv2-pool listening");

    let share_target_fallback = leading_zero_bits_target(args.share_leading_bits);
    let vardiff = if args.vardiff_target_seconds == 0 {
        None
    } else {
        Some(VardiffConfig {
            target_interval_secs: args.vardiff_target_seconds as f64,
            window: if args.vardiff_window_seconds == 0 {
                None
            } else {
                Some(Duration::from_secs(args.vardiff_window_seconds))
            },
        })
    };
    info!(
        vardiff_target_seconds = args.vardiff_target_seconds,
        vardiff_window_seconds = args.vardiff_window_seconds,
        share_leading_bits = args.share_leading_bits,
        "share difficulty policy"
    );
    let ledger = Arc::new(Ledger::default());
    // Pool-wide accepted-share dedup: a header hash is credited at most
    // once, across ALL channels. Rejects both same-channel resubmission
    // (PPLNS weight farming) and identical work found twice. 65_536
    // entries ≈ days of share traffic at live rates — far beyond any
    // window where a legitimate duplicate could exist.
    let dedup = Arc::new(Mutex::new(ShareDedup::new(65_536)));
    // Per-connection channel id allocator. Channel 1 is reserved as the
    // historical default; new connections take 2, 3, … so pool logs and
    // future SetTarget routing can disambiguate miners on the wire.
    let next_channel_id = Arc::new(AtomicU32::new(2));

    /// Decrements the live-miner gauge however the connection task ends
    /// — clean disconnect, error return, or panic. A plain counter with
    /// a decrement at the end of the happy path would drift upward on
    /// every abnormal exit and quietly overstate the pool.
    struct ConnGauge(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ConnGauge {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::Relaxed);
        }
    }
    let connected_miners = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    // PPLNS shared-mode state: a rolling window of recent share credits
    // (14_400s target span) restored from the on-disk journal, plus the
    // journal itself for ongoing appends. Losing the journal only costs
    // unpaid share *credit* — never funds (see journal.rs).
    let window = Arc::new(Mutex::new(PplnsWindow::restore(
        WindowJournal::load(&args.pplns_journal).unwrap_or_else(|e| {
            warn!(
                error = %e,
                "PPLNS journal unreadable — starting with an empty window"
            );
            Vec::new()
        }),
        14_400,
    )));
    let journal = Arc::new(Mutex::new(
        WindowJournal::open(&args.pplns_journal).context("opening PPLNS journal")?,
    ));

    // Payout address: file beats flag (see payout.rs). Held in a watch so the
    // template producer picks up a runtime change on its next iteration —
    // no restart, and no chance of a half-applied swap.
    // `required_unless_present` guarantees this past the --print-pubkey exit.
    let payout_flag = args
        .payout_address
        .clone()
        .context("--payout-address is required to run the pool")?;
    let (payout_addr_str, from_file) =
        payout::resolve_startup(&payout_flag, &args.payout_address_file);
    if from_file {
        info!(
            address = %payout_addr_str,
            path = %args.payout_address_file.display(),
            "payout address restored from disk (overrides --payout-address)"
        );
    }
    let (payout_tx, payout_rx) = watch::channel::<String>(payout_addr_str);
    if args.shared_fee_bps > fee::MAX_BPS {
        anyhow::bail!("--shared-fee-bps must be between 0 and 10000");
    }
    let (shared_fee_bps, fee_from_file) =
        fee::resolve_startup(args.shared_fee_bps, &args.shared_fee_bps_file);
    if fee_from_file {
        info!(
            fee_bps = shared_fee_bps,
            path = %args.shared_fee_bps_file.display(),
            "operator fee restored from disk (overrides --shared-fee-bps)"
        );
    }
    let (fee_tx, fee_rx) = watch::channel::<u32>(shared_fee_bps);

    let (tx, rx) = watch::channel::<Option<Arc<TemplateBundle>>>(None);

    // Liveness stamp for the template producer. Monotonic (Instant-based),
    // so a clock step can neither trip the watchdog nor mask a wedge.
    let heartbeat = Arc::new(supervisor::Heartbeat::new());

    // Template producer task.
    let mut template_producer = {
        let backends = backends.clone();
        let payout_rx = payout_rx.clone();
        let fee_rx = fee_rx.clone();
        let poll = Duration::from_secs(args.poll_secs);
        let refresh_same_tip = if args.refresh_same_tip_secs == 0 {
            None
        } else {
            Some(Duration::from_secs(args.refresh_same_tip_secs))
        };
        // Renamed (not `window`) to avoid shadowing the `refresh_same_tip`
        // match arm's `window: Duration` binding a few lines below.
        let pplns_window = window.clone();
        let shared_max_outputs = args.shared_max_outputs;
        let shared_dust_una = args.shared_dust_una;
        let utreexo_maturity_leaf_height = args.utreexo_maturity_leaf_height;
        let heartbeat = heartbeat.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(poll);
            let mut last_tip: Option<String> = None;
            let mut last_template_at: Option<std::time::Instant> = None;
            let mut last_nbits: Option<u32> = None;
            let mut last_backend_epoch: Option<u64> = None;
            let mut template_id: u64 = 0;
            loop {
                heartbeat.beat(supervisor::Phase::Sleeping);
                ticker.tick().await;
                // Upstream folded the tip poll and the template fetch into one
                // call, so this single await covers what used to be two phases.
                heartbeat.beat(supervisor::Phase::PollingTip);
                // Re-read every iteration so a runtime change lands on the
                // very next template rather than waiting for a restart.
                let payout = payout_rx.borrow().clone();
                let (backend, gbt) = match backends.select_template(&payout).await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(error = %e, "no mining-safe backend; miners retain their last job while failover retries");
                        continue;
                    }
                };
                let rpc = backend.client.clone();
                let tip = backend.health.best_hash.clone();
                let backend_changed = last_backend_epoch != Some(backend.epoch);
                let tip_changed = last_tip.as_deref() != Some(tip.as_str());
                let stale_same_tip = match (refresh_same_tip, last_template_at) {
                    (Some(window), Some(t)) => t.elapsed() >= window,
                    (Some(_), None) => true,
                    (None, _) => false,
                };
                if !tip_changed && !stale_same_tip && !backend_changed {
                    continue;
                }
                // Past the early-out: from here we actually build and publish.
                heartbeat.beat(supervisor::Phase::FetchingTemplate);
                template_id = template_id.wrapping_add(1);
                let mut pt = match mapper::map_template(&gbt, template_id) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!(error = %e, "map_template failed");
                        continue;
                    }
                };
                match rpc.get_utreexo_roots().await {
                    Ok(v) => match mapper::map_utreexo_roots(&v) {
                        Ok(s) => {
                            debug!(
                                num_leaves = s.num_leaves,
                                num_roots = s.forest_roots.len(),
                                "utreexo pre-block state fetched"
                            );
                            pt.utreexo_pre_block = Some(s);
                        }
                        Err(e) => warn!(error = %e, "map_utreexo_roots failed"),
                    },
                    Err(e) => {
                        warn!(error = %e, "getutreexoroots failed — JD miners won't be able to recompute utreexo_root");
                    }
                }

                // Phase 6 mempool inclusion: apply mempool tx
                // deletions+additions to the chain-tip pre-block state
                // to derive the pre-coinbase state. Without this, JD
                // miners can't reconstruct the right utreexo_root when
                // mempool txs are in the block. If anything fails here
                // we drop the mempool txs and fall back to a coinbase-
                // only template (the pool stays alive).
                if !pt.mempool_txs.is_empty() {
                    if let Some(pre_block) = pt.utreexo_pre_block.as_ref().cloned() {
                        match apply_mempool_to_pre_coinbase(
                            rpc.as_ref(),
                            &pre_block,
                            &pt.mempool_txs,
                            pt.height,
                            utreexo_maturity_leaf_height,
                        )
                        .await
                        {
                            Ok(post) => {
                                debug!(
                                    pre_leaves = pre_block.num_leaves,
                                    post_leaves = post.num_leaves,
                                    mempool_tx_count = pt.mempool_txs.len(),
                                    "post-mempool utreexo state derived"
                                );
                                pt.utreexo_pre_block = Some(post);
                            }
                            Err(e) => {
                                warn!(
                                    error = %e,
                                    mempool_tx_count = pt.mempool_txs.len(),
                                    "post-mempool utreexo derivation failed — \
                                     dropping mempool txs from this template"
                                );
                                // Coinbase-only fallback: drop the
                                // mempool tx list, restore the merkle
                                // root to the bare coinbase txid (the
                                // header leaf for a single-tx block),
                                // and let JD miners build their own
                                // coinbase on the unmodified
                                // pre-block utreexo state.
                                pt.mempool_txs.clear();
                                pt.merkle_path.clear();
                                pt.wire.merkle_root = pt.coinbase_txid_raw;
                            }
                        }
                    }
                }
                let nbits_changed = last_nbits != Some(pt.wire.difficulty);
                // Whether SOLO miners need a fresh push_job: only on an
                // actual tip or nbits change — matches pre-Task-7
                // behaviour exactly. Deliberately NOT set on
                // `stale_same_tip` alone: reaching this point already
                // implies `tip_changed || stale_same_tip` (see the gate
                // above), so a bundle is always built and published from
                // here on. That's required because `stale_same_tip` is
                // the ONLY scheduled point where a live PPLNS window
                // snapshot (weights change on every accepted shared
                // share, with no tip/nbits signal at all) gets baked
                // into a fresh `shared` coinbase below — freezing that
                // would stall shared-mode payouts. But solo miners have
                // no reason to be re-pushed an identical job on that
                // tick (spurious SetNewPrevHash + "new tip" UI churn on
                // idle chains), so `solo_changed` lets `serve_miner`
                // skip the solo push (and skip rebasing `current` onto
                // this bundle) while still refreshing shared jobs every
                // window.
                let solo_changed = tip_changed || nbits_changed || backend_changed;
                info!(
                    template_id = pt.wire.template_id,
                    tip = %tip,
                    nbits = format!("0x{:08x}", pt.wire.difficulty),
                    nbits_changed,
                    tip_changed,
                    backend = %backend.health.endpoint,
                    backend_epoch = backend.epoch,
                    backend_changed,
                    chainwork = %backend.health.chainwork,
                    block_target = %hex::encode(pt.block_target),
                    utreexo_leaves = pt.utreexo_pre_block.as_ref().map(|s| s.num_leaves),
                    "new template"
                );
                // Build the shared-mode variant on top of the same
                // template. Fee script is derived from the daemon's own
                // coinbase (the pool's --payout-address output) rather
                // than a new flag — cached per template since the
                // coinbase changes every refresh. Any failure here (no
                // non-OP_RETURN output found, or the split builder
                // erroring) just leaves `shared = None`: shared miners
                // get no new job until the next successful refresh, but
                // solo mining and the pool itself are unaffected.
                let shared_split = match mapper::extract_fee_script(&pt.coinbase_full_hex) {
                    Ok(fee_script) => {
                        // Snapshot the weights and release the window lock
                        // immediately — compute_split/merge_duplicate_outputs
                        // don't need it, and holding it here would block
                        // shared-share crediting (which also takes this
                        // lock) for the duration of split computation.
                        let weights = {
                            let w = pplns_window.lock().unwrap();
                            w.weights()
                        };
                        let params = split::SplitParams {
                            reward_una: pt.coinbase_value_una,
                            fee_bps: *fee_rx.borrow(),
                            fee_script: &fee_script,
                            max_outputs: shared_max_outputs,
                            dust_una: shared_dust_una,
                            // No finder yet at template time — use the
                            // fee script so an empty-window template
                            // still validates (sums correctly). The
                            // finder-specific split only matters at
                            // block-submit time, which this template
                            // doesn't need to know about.
                            finder_script: &fee_script,
                        };
                        let split_outputs =
                            split::merge_duplicate_outputs(split::compute_split(&weights, &params));
                        // Pre-flight an extranonce-free build so a split
                        // that can't produce a valid template is logged
                        // ONCE here instead of once per channel. The
                        // per-channel builds in `serve_miner` reuse these
                        // exact split outputs with the channel's own
                        // scriptSig extranonce.
                        match shared_template::build_shared_template(
                            &pt,
                            split_outputs.clone(),
                            None,
                            utreexo_maturity_leaf_height,
                        ) {
                            Ok(_) => Some(split_outputs),
                            Err(e) => {
                                warn!(error = %e, "build_shared_template failed — shared miners get no job this refresh");
                                None
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "extract_fee_script failed — shared miners get no job this refresh");
                        None
                    }
                };
                ops::telemetry().record_template(
                    &backend.health.endpoint,
                    backend.health.blocks,
                    backend.health.headers,
                    u64::from(pt.height),
                    pt.wire.template_id,
                    hex::encode(pt.wire.prev_block_hash),
                );
                let _ = tx.send(Some(Arc::new(TemplateBundle {
                    pt: pt.clone(),
                    shared_split,
                    solo_changed,
                    backend_epoch: backend.epoch,
                    backend_endpoint: backend.health.endpoint.clone(),
                })));
                heartbeat.beat(supervisor::Phase::Publishing);
                last_tip = Some(tip);
                last_template_at = Some(std::time::Instant::now());
                last_nbits = Some(pt.wire.difficulty);
                last_backend_epoch = Some(backend.epoch);
            }
        })
    };

    // Read-only operator status endpoint. Failing to start it is fatal
    // rather than a warning: an operator who configured it and got a
    // silently-dead endpoint would be worse off than one told why.
    if !args.ops_bind.trim().is_empty() {
        let token = ops::load_token(&args.ops_token_file)?;
        let listener = ops::bind(args.ops_bind.trim()).await?;
        let started = std::time::Instant::now();
        let ops_window = window.clone();
        let ops_ledger = ledger.clone();
        let ops_heartbeat = heartbeat.clone();
        let ops_connected = connected_miners.clone();
        let ops_templates = rx.clone();
        let stratum_bind = args.bind.to_string();
        let ops_fee_rx = fee_rx.clone();
        let ops_payout_rx = payout_rx.clone();
        let snapshot = Arc::new(move || {
            let (entries, span, miners) = {
                let w = ops_window.lock().unwrap();
                let total = w.total_weight();
                let span = w
                    .entries()
                    .next()
                    .zip(w.entries().last())
                    .map(|(f, l)| l.unix_ts.saturating_sub(f.unix_ts))
                    .unwrap_or(0);
                let mut per: std::collections::HashMap<Vec<u8>, u128> = Default::default();
                for e in w.entries() {
                    *per.entry(e.payout_script.clone()).or_insert(0) += e.weight;
                }
                let mut rows: Vec<ops::MinerStatus> = per
                    .into_iter()
                    .map(|(script, weight)| ops::MinerStatus {
                        payout_script_hex: hex::encode(&script),
                        bps: if total == 0 {
                            0
                        } else {
                            ((weight.saturating_mul(10_000)) / total) as u32
                        },
                        window_weight: weight.to_string(),
                    })
                    .collect();
                rows.sort_by(|a, b| b.bps.cmp(&a.bps));
                (w.len(), span, rows)
            };
            let credits = ops_ledger.snapshot();
            let telemetry = ops::telemetry().snapshot();
            const TEMPLATE_STALE_SECS: u64 = 120;
            ops::OpsStatus {
                schema_version: 2,
                generated_at_unix: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                payout_address: ops_payout_rx.borrow().clone(),
                pool_version: env!("CARGO_PKG_VERSION").to_string(),
                uptime_secs: started.elapsed().as_secs(),
                fee_bps: *ops_fee_rx.borrow(),
                connected_miners: ops_connected.load(Ordering::Relaxed),
                window_entries: entries,
                window_span_secs: span,
                template_heartbeat_age_secs: ops_heartbeat.age_secs(),
                template_phase: ops_heartbeat.phase().as_str().to_string(),
                accepted_shares_total: telemetry.accepted,
                rejected_shares_total: telemetry.rejected,
                blocks_found_total: credits.values().map(|c| c.found_blocks).sum(),
                miners,
                stratum_bind: stratum_bind.clone(),
                daemon_connected: ops_templates.borrow().is_some()
                    && ops_heartbeat.age_secs() <= TEMPLATE_STALE_SECS,
                daemon_endpoint: telemetry.daemon_endpoint,
                daemon_blocks: telemetry.daemon_blocks,
                daemon_headers: telemetry.daemon_headers,
                template_height: telemetry.last_template_height,
                template_id: telemetry.last_template_id,
                template_prev_hash: telemetry.last_template_hash,
                last_template_at_unix: telemetry.last_template_at_unix,
                last_share: telemetry.last_share,
                last_block: telemetry.last_block,
                rejection_reasons: telemetry.rejection_reasons,
            }
        });
        let policy = ops::Policy {
            allow_payout_change: args.ops_allow_payout_change,
            allow_fee_change: args.ops_allow_fee_change,
        };
        if policy.allow_payout_change {
            warn!(
                "ops endpoint accepts payout-address changes: the ops token can \
                 now redirect YOUR fee output (miners' payouts are unaffected)"
            );
        }
        if policy.allow_fee_change {
            warn!(
                "ops endpoint accepts operator-fee changes: the ops token can alter YOUR fee percentage for future templates"
            );
        }
        let apply_backends = backends.clone();
        let apply_path = args.payout_address_file.clone();
        let apply_tx = Arc::new(payout_tx);
        let apply_payout = Arc::new(move |candidate: String| {
            let backends = apply_backends.clone();
            let path = apply_path.clone();
            let tx = apply_tx.clone();
            async move {
                // 1. Cheap syntactic gate, so an obvious typo costs no RPC.
                if !ops::looks_like_payout_address(&candidate) {
                    return Err("not a plausible din1p... address".to_string());
                }
                // 2. Authoritative check. A well-formed but wrong address makes
                //    getblocktemplate fail, which would mean zero templates and
                //    a dead pool — so prove it produces one BEFORE adopting it.
                //    On failure the old address is still live and miners never
                //    see a gap.
                if let Err(e) = backends.select_template(&candidate).await {
                    return Err(format!("node refused a template for that address: {e}"));
                }
                // 3. Persist before swapping. If the write fails we must not
                //    end up live on an address that a restart would revert.
                if let Err(e) = payout::store(&path, &candidate) {
                    return Err(format!("could not persist the address: {e}"));
                }
                // 4. Swap. The producer picks this up on its next iteration.
                if tx.send(candidate.clone()).is_err() {
                    return Err("template producer is gone".to_string());
                }
                Ok(candidate)
            }
        });
        let apply_fee_path = args.shared_fee_bps_file.clone();
        let apply_fee_tx = Arc::new(fee_tx);
        let apply_fee = Arc::new(move |candidate: u32| {
            let path = apply_fee_path.clone();
            let tx = apply_fee_tx.clone();
            async move {
                if candidate > fee::MAX_BPS {
                    return Err("operator fee must be between 0 and 10000 basis points".to_string());
                }
                if let Err(error) = fee::store(&path, candidate) {
                    return Err(format!("could not persist the operator fee: {error}"));
                }
                if tx.send(candidate).is_err() {
                    return Err("template producer is gone".to_string());
                }
                Ok(candidate)
            }
        });
        tokio::spawn(async move {
            if let Err(e) =
                ops::serve(listener, token, policy, snapshot, apply_payout, apply_fee).await
            {
                warn!(error = %e, "ops endpoint stopped");
            }
        });
    }

    // Miner acceptor.
    //
    // Two independent ways the template producer can stop serving:
    //   - it EXITS (returns or panics)  -> `&mut template_producer` resolves
    //   - it WEDGES (alive but stuck)   -> the handle never resolves, so
    //     only a stale heartbeat can reveal it
    // Both bail out of main, which exits non-zero so systemd restarts us.
    let mut stall_check = tokio::time::interval(Duration::from_secs(30));
    stall_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let (sock, peer) = tokio::select! {
            result = &mut template_producer => {
                match result {
                    Ok(()) => anyhow::bail!("template producer exited unexpectedly"),
                    Err(e) => return Err(anyhow::Error::new(e)
                        .context("template producer task failed")),
                }
            }
            _ = stall_check.tick() => {
                if args.template_stall_secs > 0
                    && heartbeat.is_stalled(args.template_stall_secs)
                {
                    anyhow::bail!(
                        "template producer wedged: no heartbeat for {}s (limit {}s, last phase: {}). \
                         Exiting so systemd restarts the pool rather than serving stale jobs.",
                        heartbeat.age_secs(),
                        args.template_stall_secs,
                        heartbeat.phase().as_str()
                    );
                }
                continue;
            }
            accepted = listener.accept() => accepted?,
        };
        let rx = rx.clone();
        let backends = backends.clone();
        let ledger = ledger.clone();
        let share_target_copy = share_target_fallback;
        let keys = static_keys.clone();
        let channel_id = next_channel_id.fetch_add(1, Ordering::Relaxed);
        let conn_gauge = connected_miners.clone();
        let vardiff_copy = vardiff;
        let window = window.clone();
        let journal = journal.clone();
        let dedup = dedup.clone();
        let utreexo_maturity_leaf_height = args.utreexo_maturity_leaf_height;
        tokio::spawn(async move {
            conn_gauge.fetch_add(1, Ordering::Relaxed);
            let _gauge = ConnGauge(conn_gauge);
            info!(%peer, channel_id, "miner connected — handshake starting");
            let session = match NoiseSession::accept_nx(sock, &keys).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%peer, error = %e, "noise handshake failed");
                    return;
                }
            };
            let miner_key = session.peer_static_key();
            info!(%peer, channel_id, miner = %hex::encode(miner_key), "noise handshake complete");
            if let Err(e) = serve_miner(
                session,
                rx,
                share_target_copy,
                vardiff_copy,
                miner_key,
                backends,
                ledger,
                channel_id,
                window,
                journal,
                dedup,
                utreexo_maturity_leaf_height,
            )
            .await
            {
                warn!(%peer, channel_id, error = %e, "miner session ended with error");
            } else {
                info!(%peer, channel_id, "miner disconnected");
            }
        });
    }
}

fn default_cookie_path() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{home}/.dinero/.cookie")
}

fn default_pool_key_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(format!("{home}/.dinero/dinero-sv2-pool.key"))
}

#[allow(clippy::too_many_arguments)]
async fn serve_miner(
    mut session: NoiseSession<TcpStream>,
    mut rx: watch::Receiver<Option<Arc<TemplateBundle>>>,
    share_target_fallback: [u8; 32],
    vardiff: Option<VardiffConfig>,
    miner_key: MinerKey,
    backends: Arc<BackendPool>,
    ledger: Arc<Ledger>,
    channel_id: u32,
    window: Arc<Mutex<PplnsWindow>>,
    journal: Arc<Mutex<WindowJournal>>,
    dedup: Arc<Mutex<ShareDedup>>,
    utreexo_maturity_leaf_height: u32,
) -> Result<()> {
    // ---- Phase A: SetupConnection ----
    let f = session
        .read_frame()
        .await?
        .context("EOF before SetupConnection")?;
    if f.msg_type != MSG_SETUP_CONNECTION {
        warn!(msg_type = f.msg_type, "expected SetupConnection");
        return Ok(());
    }
    let setup = match decode_setup_connection(&f.payload) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "bad SetupConnection payload");
            let err = SetupConnectionError {
                flags: 0,
                error_code: b"invalid-payload".to_vec(),
            };
            session
                .write_frame(
                    MSG_SETUP_CONNECTION_ERROR,
                    &encode_setup_connection_error(&err)?,
                )
                .await?;
            return Ok(());
        }
    };
    if setup.protocol != PROTOCOL_MINING {
        let err = SetupConnectionError {
            flags: 0,
            error_code: b"unsupported-protocol".to_vec(),
        };
        session
            .write_frame(
                MSG_SETUP_CONNECTION_ERROR,
                &encode_setup_connection_error(&err)?,
            )
            .await?;
        return Ok(());
    }
    // Simple range check: PROTOCOL_VERSION must fall within the miner's
    // declared [min, max]. Otherwise the dialects are incompatible.
    if PROTOCOL_VERSION < setup.min_version || PROTOCOL_VERSION > setup.max_version {
        let err = SetupConnectionError {
            flags: 0,
            error_code: b"version-incompatible".to_vec(),
        };
        session
            .write_frame(
                MSG_SETUP_CONNECTION_ERROR,
                &encode_setup_connection_error(&err)?,
            )
            .await?;
        return Ok(());
    }
    session
        .write_frame(
            MSG_SETUP_CONNECTION_SUCCESS,
            &encode_setup_connection_success(&SetupConnectionSuccess {
                used_version: PROTOCOL_VERSION,
                flags: 0,
            }),
        )
        .await?;
    info!(
        user_agent = %String::from_utf8_lossy(&setup.user_agent),
        "SetupConnection OK"
    );

    // ---- Phase B: OpenStandardMiningChannel ----
    let f = session
        .read_frame()
        .await?
        .context("EOF before OpenStandardMiningChannel")?;
    if f.msg_type != MSG_OPEN_STANDARD_MINING_CHANNEL {
        warn!(msg_type = f.msg_type, "expected OpenStandardMiningChannel");
        return Ok(());
    }
    let open = match decode_open_standard_mining_channel(&f.payload) {
        Ok(o) => o,
        Err(e) => {
            warn!(error = %e, "bad OpenStandardMiningChannel payload");
            let err = OpenStandardMiningChannelError {
                request_id: 0,
                error_code: b"invalid-payload".to_vec(),
            };
            session
                .write_frame(
                    MSG_OPEN_STANDARD_MINING_CHANNEL_ERROR,
                    &encode_open_standard_mining_channel_error(&err)?,
                )
                .await?;
            return Ok(());
        }
    };
    // Vardiff: size the channel's initial target from the miner's
    // declared `nominal_hash_rate_bits` (a Hz value packed as f32 bits),
    // aiming for ~1 share per `target_interval_secs`. If the miner
    // reports a 0 / NaN / negative rate, or vardiff is disabled, fall
    // back to the pool default.
    let initial_share_target = match vardiff {
        Some(cfg) => {
            let rate_hps = f32::from_bits(open.nominal_hash_rate_bits) as f64;
            let t = target_for_hashrate(rate_hps, cfg.target_interval_secs);
            // Clamp: never give a channel an EASIER target than the
            // pool's default fallback. A miner reporting 0 hashrate
            // shouldn't get an "every hash is a share" target.
            if t > share_target_fallback {
                share_target_fallback
            } else {
                t
            }
        }
        None => share_target_fallback,
    };
    let mut share_target = initial_share_target;
    info!(
        channel_id,
        nominal_hps = f32::from_bits(open.nominal_hash_rate_bits),
        vardiff = vardiff.is_some(),
        initial_target = %hex::encode(initial_share_target),
        "channel-open vardiff sizing",
    );
    // Miner's max_target must be ≥ the pool's assigned share target;
    // otherwise the miner's hardware can't produce shares we'd accept.
    if open.max_target < share_target {
        let err = OpenStandardMiningChannelError {
            request_id: open.request_id,
            error_code: b"max-target-too-low".to_vec(),
        };
        session
            .write_frame(
                MSG_OPEN_STANDARD_MINING_CHANNEL_ERROR,
                &encode_open_standard_mining_channel_error(&err)?,
            )
            .await?;
        return Ok(());
    }
    session
        .write_frame(
            MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
            &encode_open_standard_mining_channel_success(&OpenStandardMiningChannelSuccess {
                request_id: open.request_id,
                channel_id,
                target: share_target,
            }),
        )
        .await?;
    info!(
        channel_id,
        user_identity = %String::from_utf8_lossy(&open.user_identity),
        "channel open"
    );

    // ---- Phase C: normal operation ----
    let mut current: Option<Arc<TemplateBundle>> = None;
    // This channel's OWN shared template (per-channel scriptSig
    // extranonce baked in), derived from the bundle at each push so
    // share validation always matches the header the miner is grinding.
    let mut current_shared: Option<Arc<SharedTemplate>> = None;
    let mut last_sequence_number: u32 = 0;
    // None = solo; Some(payout_script) = shared mode. Modern miners send
    // SetRewardMode immediately after channel-open success. Wait briefly
    // for that declaration before sending any work so a shared miner never
    // receives (or displays) a misleading solo bootstrap job. Legacy miners
    // that do not implement SetRewardMode retain a bounded solo fallback.
    let mut reward_mode: Option<Vec<u8>> = None;

    match tokio::time::timeout(Duration::from_secs(2), session.read_frame()).await {
        Ok(Ok(Some(f))) if f.msg_type == MSG_SET_REWARD_MODE => {
            match decode_set_reward_mode(&f.payload) {
                Ok(m) if m.mode == 1 => {
                    if m.payout_script.len() == 34
                        && m.payout_script[0] == 0x51
                        && m.payout_script[1] == 0x20
                    {
                        info!(
                            channel_id,
                            payout = %hex::encode(&m.payout_script),
                            "miner opened in SHARED mode"
                        );
                        reward_mode = Some(m.payout_script);
                    } else {
                        send_share_error(&mut session, channel_id, 0, "bad-payout-script").await?;
                        return Ok(());
                    }
                }
                Ok(_) => {
                    info!(channel_id, "miner explicitly opened in SOLO mode");
                }
                Err(e) => {
                    warn!(error = %e, "bad initial SetRewardMode payload");
                    send_share_error(&mut session, channel_id, 0, "bad-payload").await?;
                    return Ok(());
                }
            }
        }
        Ok(Ok(Some(f))) => {
            warn!(
                msg_type = f.msg_type,
                "expected SetRewardMode before first mining job"
            );
            return Ok(());
        }
        Ok(Ok(None)) => return Ok(()),
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            info!(
                channel_id,
                "legacy miner did not declare reward mode; defaulting to SOLO"
            );
        }
    }

    let initial = rx.borrow_and_update().clone();
    if let Some(bundle) = initial {
        debug!(
            backend = %bundle.backend_endpoint,
            backend_epoch = bundle.backend_epoch,
            template_id = bundle.pt.wire.template_id,
            "installing initial backend job generation"
        );
        match &reward_mode {
            Some(payout_script) => {
                if let Some(st) =
                    derive_channel_shared(&bundle, channel_id, utreexo_maturity_leaf_height)
                {
                    push_shared_job(&mut session, channel_id, &st, &window, payout_script).await?;
                    current_shared = Some(st);
                }
            }
            None => push_job(&mut session, channel_id, &bundle.pt).await?,
        }
        current = Some(bundle);
    }

    // Vardiff measurement: count accepted shares since the last
    // retarget tick, recompute observed rate, emit MSG_SET_TARGET if
    // the new sizing is materially different from the current target.
    // Disabled when `vardiff.window` is None or vardiff itself is None.
    let mut accepted_in_window: u64 = 0;
    let mut window_start = std::time::Instant::now();
    let vardiff_window = vardiff.and_then(|v| v.window);
    let mut vardiff_tick = vardiff_window.map(|w| {
        let mut t = tokio::time::interval(w);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // First tick fires immediately; consume it so we wait a
        // full window before our first measurement.
        t
    });

    loop {
        tokio::select! {
            biased;

            changed = rx.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let maybe_bundle = rx.borrow_and_update().clone();
                if let Some(bundle) = maybe_bundle {
                    debug!(
                        backend = %bundle.backend_endpoint,
                        backend_epoch = bundle.backend_epoch,
                        template_id = bundle.pt.wire.template_id,
                        "observed backend job generation"
                    );
                    match &reward_mode {
                        None => {
                            // Skip both the push AND the `current` swap
                            // when the solo template didn't materially
                            // change this refresh (a `stale_same_tip`
                            // tick that only exists to refresh SHARED
                            // weights — see `TemplateBundle::solo_changed`).
                            // Rebasing `current` here without pushing a
                            // job would desync share validation from
                            // what the miner is actually hashing (still
                            // the old merkle_root/mempool set), silently
                            // failing valid — even block-worthy — shares.
                            if bundle.solo_changed {
                                push_job(&mut session, channel_id, &bundle.pt).await?;
                                current = Some(bundle);
                            }
                        }
                        Some(payout_script) => {
                            // If the shared build failed this refresh
                            // (logged in the producer), skip the push —
                            // the miner keeps its last job until the
                            // next successful refresh. Never crash.
                            if let Some(st) = derive_channel_shared(
                                &bundle,
                                channel_id,
                                utreexo_maturity_leaf_height,
                            ) {
                                push_shared_job(&mut session, channel_id, &st, &window, payout_script).await?;
                                current_shared = Some(st);
                                // Keep the daemon-derived block target paired
                                // with the exact per-channel template that was
                                // actually sent. On a failed refresh the miner
                                // continues its previous job and validation must
                                // continue using that previous bundle too.
                                current = Some(bundle);
                            }
                        }
                    }
                }
            }

            // Vardiff retargeting: only armed when configured. Sized so
            // the next-emitted target produces ~1 share / target_interval
            // at the OBSERVED rate (smoothed against the last setting).
            _ = async {
                match vardiff_tick.as_mut() {
                    Some(t) => { t.tick().await; }
                    None => std::future::pending::<()>().await,
                }
            }, if vardiff_tick.is_some() => {
                if let Some(cfg) = vardiff {
                    let elapsed = window_start.elapsed().as_secs_f64().max(0.001);
                    let observed_share_rate = accepted_in_window as f64 / elapsed;
                    if observed_share_rate > 0.0 {
                        // observed_share_rate (shares/sec) under the CURRENT
                        // target T means hashrate ≈ shares/sec × 2²⁵⁶ / T,
                        // and a ~1-share-per-interval target needs hashrate
                        // × interval. Easier to express in terms of the
                        // current target shape: new_target = current_target ×
                        // (shares observed) / (shares we wanted).
                        //
                        // But we already track hashrate via the miner's
                        // declared `nominal_hash_rate_bits` at open. After
                        // one window we have a much better number:
                        //   hashrate = (shares × 2²⁵⁶ / current_target) /
                        //              elapsed
                        // For the leading-zero-target shape, that simplifies
                        // to a small integer adjustment in `bits`. Recompute
                        // from observed rate directly via `target_for_hashrate`.
                        let leading_zero_bits_in_current =
                            count_leading_zero_bits(&share_target) as f64;
                        let work_per_share = 2f64.powf(leading_zero_bits_in_current);
                        let inferred_hashrate = observed_share_rate * work_per_share;
                        let new_target = target_for_hashrate(
                            inferred_hashrate,
                            cfg.target_interval_secs,
                        );
                        // Clamp easier-than-fallback (paranoia).
                        let new_target = if new_target > share_target_fallback {
                            share_target_fallback
                        } else {
                            new_target
                        };
                        if new_target != share_target {
                            info!(
                                channel_id,
                                accepted_in_window,
                                window_secs = elapsed,
                                observed_rate_per_sec = observed_share_rate,
                                inferred_hashrate_hps = inferred_hashrate,
                                new_target = %hex::encode(new_target),
                                old_target = %hex::encode(share_target),
                                "vardiff retarget"
                            );
                            share_target = new_target;
                            let payload = encode_set_target(
                                &dinero_sv2_common::SetTarget {
                                    channel_id,
                                    max_target: share_target,
                                },
                            );
                            session.write_frame(MSG_SET_TARGET, &payload).await?;
                        }
                    }
                    accepted_in_window = 0;
                    window_start = std::time::Instant::now();
                }
            }

            frame = session.read_frame() => {
                let f = match frame? {
                    Some(f) => f,
                    None => return Ok(()),
                };
                let Frame { msg_type: mtype, payload, .. } = f;
                match mtype {
                    MSG_SET_REWARD_MODE => {
                        match decode_set_reward_mode(&payload) {
                            Ok(m) if m.mode == 1 => {
                                // Shape check: 34-byte taproot script (0x51 0x20 …).
                                if m.payout_script.len() == 34
                                    && m.payout_script[0] == 0x51 && m.payout_script[1] == 0x20 {
                                    info!(
                                        channel_id,
                                        payout = %hex::encode(&m.payout_script),
                                        "miner switched to SHARED mode"
                                    );
                                    reward_mode = Some(m.payout_script.clone());
                                    // Push the current shared job immediately so the
                                    // miner doesn't idle until the next refresh.
                                    if let Some(bundle) = current.as_ref() {
                                        if let Some(st) = derive_channel_shared(
                                            bundle,
                                            channel_id,
                                            utreexo_maturity_leaf_height,
                                        ) {
                                            push_shared_job(&mut session, channel_id, &st, &window, &m.payout_script).await?;
                                            current_shared = Some(st);
                                        }
                                    }
                                } else {
                                    send_share_error(&mut session, channel_id, 0, "bad-payout-script").await?;
                                }
                            }
                            Ok(_) => { reward_mode = None; } // explicit solo
                            Err(e) => {
                                warn!(error = %e, "bad SetRewardMode payload");
                                send_share_error(&mut session, channel_id, 0, "bad-payload").await?;
                            }
                        }
                    }
                    MSG_SUBMIT_SHARES_STANDARD => {
                        match &reward_mode {
                            None => {
                                handle_share(
                                    &mut session,
                                    &payload,
                                    current.as_ref().map(|b| &b.pt),
                                    share_target,
                                    channel_id,
                                    &mut last_sequence_number,
                                    &mut accepted_in_window,
                                    miner_key,
                                    backends.as_ref(),
                                    ledger.as_ref(),
                                    &dedup,
                                )
                                .await?;
                            }
                            Some(payout_script) => {
                                let payout_script = payout_script.clone();
                                handle_shared_share(
                                    &mut session,
                                    &payload,
                                    current.as_ref(),
                                    current_shared.as_ref(),
                                    share_target,
                                    channel_id,
                                    &mut last_sequence_number,
                                    &mut accepted_in_window,
                                    &payout_script,
                                    backends.as_ref(),
                                    ledger.as_ref(),
                                    &window,
                                    &journal,
                                    &dedup,
                                )
                                .await?;
                            }
                        }
                    }
                    MSG_SUBMIT_SHARES_EXTENDED => {
                        handle_extended_share(
                            &mut session,
                            &payload,
                            current.as_ref().map(|b| &b.pt),
                            share_target,
                            channel_id,
                            &mut last_sequence_number,
                            &mut accepted_in_window,
                            miner_key,
                            backends.as_ref(),
                            ledger.as_ref(),
                            &dedup,
                            utreexo_maturity_leaf_height,
                        )
                        .await?;
                    }
                    other => warn!(msg_type = other, "unexpected frame type from miner"),
                }
            }
        }
    }
}

/// Build THIS channel's shared template from the bundle's PPLNS split:
/// same payout outputs, but with the channel id spliced into the
/// coinbase scriptSig as an extranonce so the header (merkle_root +
/// utreexo_root) is unique to the channel. Channel ids are allocated
/// once per connection for the pool's lifetime, so no two live (or
/// reconnected) channels ever share a header. Returns `None` (warned)
/// when this refresh has no shared split or the build fails.
fn derive_channel_shared(
    bundle: &TemplateBundle,
    channel_id: u32,
    utreexo_maturity_leaf_height: u32,
) -> Option<Arc<SharedTemplate>> {
    let split = bundle.shared_split.as_ref()?;
    match shared_template::build_shared_template(
        &bundle.pt,
        split.clone(),
        Some(channel_id),
        utreexo_maturity_leaf_height,
    ) {
        Ok(st) => Some(Arc::new(st)),
        Err(e) => {
            warn!(
                channel_id,
                error = %e,
                "per-channel shared template build failed — channel keeps its last job"
            );
            None
        }
    }
}

/// Emit `SetNewPrevHash`, optionally `UtreexoStateAnnouncement`, then
/// `NewMiningJob` for this template.
///
/// Every push starts with `SetNewPrevHash` so miners can explicitly
/// invalidate any in-flight work on the old tip. Between that and the
/// job, the pool sends the pre-coinbase Utreexo forest state (when
/// available from dinerod) so JD-aware miners can apply their own
/// coinbase leaves and recompute the header's `utreexo_root` —
/// observable on the wire even before a miner actually diverges
/// (useful Phase 4b verification).
async fn push_job(
    session: &mut NoiseSession<TcpStream>,
    channel_id: u32,
    pt: &PoolTemplate,
) -> Result<()> {
    let snph = SetNewPrevHash {
        channel_id,
        prev_hash: pt.wire.prev_block_hash,
        min_ntime: pt.wire.timestamp,
        nbits: pt.wire.difficulty,
    };
    session
        .write_frame(MSG_SET_NEW_PREV_HASH, &encode_set_new_prev_hash(&snph))
        .await?;

    if let Some(state) = &pt.utreexo_pre_block {
        let payload = encode_utreexo_accumulator_state(state)
            .map_err(|e| anyhow::anyhow!("utreexo state encode: {e}"))?;
        session.write_frame(MSG_UTREEXO_STATE, &payload).await?;

        // When we have pre-block state, we also have the coinbase
        // fragments + height + value the miner needs for JD. Emit
        // `MSG_COINBASE_CONTEXT` so extended-share miners can assemble
        // their own coinbase.
        let ctx = CoinbaseContext {
            channel_id,
            coinbase_prefix: pt.coinbase_prefix.clone(),
            coinbase_suffix: pt.coinbase_suffix.clone(),
            merkle_path: pt.merkle_path.clone(),
            height: pt.height,
            coinbase_value_una: pt.coinbase_value_una,
        };
        let payload = encode_coinbase_context(&ctx)
            .map_err(|e| anyhow::anyhow!("coinbase context encode: {e}"))?;
        session.write_frame(MSG_COINBASE_CONTEXT, &payload).await?;
    }

    let payload = encode_new_template(&pt.wire);
    session.write_frame(MSG_NEW_MINING_JOB, &payload).await?;
    debug!(
        template_id = pt.wire.template_id,
        utreexo_leaves = pt.utreexo_pre_block.as_ref().map(|s| s.num_leaves),
        "pushed SNPH + (utreexo + ctx) + job"
    );
    Ok(())
}

/// Shared-mode counterpart of `push_job`: the pool already owns the
/// whole coinbase (built in the producer loop via
/// `shared_template::build_shared_template`), so there's no
/// `MSG_UTREEXO_STATE` / `MSG_COINBASE_CONTEXT` to send — those are
/// JD-only, for miners that assemble their own coinbase. Followed by
/// `MSG_WINDOW_STATUS` so the miner can see its live PPLNS standing.
async fn push_shared_job(
    session: &mut NoiseSession<TcpStream>,
    channel_id: u32,
    st: &SharedTemplate,
    window: &Arc<Mutex<PplnsWindow>>,
    payout_script: &[u8],
) -> Result<()> {
    let snph = SetNewPrevHash {
        channel_id,
        prev_hash: st.wire.prev_block_hash,
        min_ntime: st.wire.timestamp,
        nbits: st.wire.difficulty,
    };
    session
        .write_frame(MSG_SET_NEW_PREV_HASH, &encode_set_new_prev_hash(&snph))
        .await?;
    session
        .write_frame(MSG_NEW_MINING_JOB, &encode_new_template(&st.wire))
        .await?;
    let (bps, shares) = {
        let w = window.lock().expect("pplns window mutex");
        (w.miner_bps(payout_script), w.len() as u64)
    };
    let ws = WindowStatus {
        channel_id,
        window_bps: bps,
        window_shares: shares,
    };
    session
        .write_frame(MSG_WINDOW_STATUS, &encode_window_status(&ws)?)
        .await?;
    debug!(
        template_id = st.wire.template_id,
        window_bps = bps,
        window_shares = shares,
        "pushed shared SNPH + job + window status"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_share(
    session: &mut NoiseSession<TcpStream>,
    payload: &[u8],
    current: Option<&PoolTemplate>,
    share_target: [u8; 32],
    channel_id: u32,
    last_sequence_number: &mut u32,
    accepted_in_window: &mut u64,
    miner_key: MinerKey,
    backends: &BackendPool,
    ledger: &Ledger,
    dedup: &Arc<Mutex<ShareDedup>>,
) -> Result<()> {
    let share = match decode_submit_shares(payload) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "bad share shape");
            ledger.reject(miner_key);
            ops::telemetry().record_rejection("invalid-payload");
            let err = SubmitSharesError {
                channel_id,
                sequence_number: *last_sequence_number,
                error_code: b"invalid-payload".to_vec(),
            };
            session
                .write_frame(MSG_SUBMIT_SHARES_ERROR, &encode_submit_shares_error(&err)?)
                .await?;
            return Ok(());
        }
    };
    *last_sequence_number = share.sequence_number;

    let Some(pt) = current else {
        warn!("share received before any template");
        ledger.reject(miner_key);
        ops::telemetry().record_rejection("no-template");
        let err = SubmitSharesError {
            channel_id,
            sequence_number: share.sequence_number,
            error_code: b"no-template".to_vec(),
        };
        session
            .write_frame(MSG_SUBMIT_SHARES_ERROR, &encode_submit_shares_error(&err)?)
            .await?;
        return Ok(());
    };

    if !job_generation::is_current_job(share.job_id, pt.wire.template_id) {
        ledger.reject(miner_key);
        send_share_error(session, channel_id, share.sequence_number, "stale-share").await?;
        return Ok(());
    }

    let hash = HeaderAssembly::hash(&pt.wire, &share);
    let meets_share = hash_meets_target(&hash, &share_target);
    let meets_block = hash_meets_target(&hash, &pt.block_target);

    if !meets_share {
        debug!(hash = %hex::encode(hash), "share below share-target");
        ledger.reject(miner_key);
        ops::telemetry().record_rejection("under-target");
        let err = SubmitSharesError {
            channel_id,
            sequence_number: share.sequence_number,
            error_code: b"under-target".to_vec(),
        };
        session
            .write_frame(MSG_SUBMIT_SHARES_ERROR, &encode_submit_shares_error(&err)?)
            .await?;
        return Ok(());
    }

    // Pool-wide dedup (solo standard channels all grind the identical
    // daemon template, so cross-channel duplicates are possible here
    // until solo work is per-channel too).
    if !dedup.lock().expect("share dedup mutex").insert(hash) {
        warn!(hash = %hex::encode(hash), channel_id, "duplicate share rejected");
        ledger.reject(miner_key);
        send_share_error(
            session,
            channel_id,
            share.sequence_number,
            "duplicate-share",
        )
        .await?;
        return Ok(());
    }

    ledger.credit_share(miner_key);
    ops::telemetry().record_accepted_share("standard", hex::encode(hash));
    *accepted_in_window += 1;
    info!(
        hash = %hex::encode(hash),
        template_id = pt.wire.template_id,
        nonce = share.nonce,
        "accepted share"
    );
    session
        .write_frame(
            MSG_SUBMIT_SHARES_SUCCESS,
            &encode_submit_shares_success(&SubmitSharesSuccess {
                channel_id,
                last_sequence_number: share.sequence_number,
                new_submits_accepted_count: 1,
                new_shares_sum: 1,
            }),
        )
        .await?;

    if meets_block {
        let mempool_data: Vec<Vec<u8>> = pt.mempool_txs.iter().map(|t| t.data.clone()).collect();
        match try_submit_block(
            &pt.wire,
            &share,
            &pt.coinbase_full_hex,
            &mempool_data,
            backends,
        )
        .await
        {
            Ok(SubmitBlockResult::Accepted) => {
                info!(
                    template_id = pt.wire.template_id,
                    hash = %hex::encode(hash),
                    "★ block accepted by dinerod"
                );
                ledger.credit_block(miner_key);
                ops::telemetry().record_block("accepted", hex::encode(hash), String::new());
            }
            Ok(SubmitBlockResult::Rejected(reason)) => {
                warn!(
                    reason,
                    hash = %hex::encode(hash),
                    "dinerod rejected our block"
                );
                ops::telemetry().record_block("rejected", hex::encode(hash), reason);
            }
            Err(e) => {
                warn!(error = %e, "submitblock RPC failed");
                ops::telemetry().record_block("error", hex::encode(hash), e.to_string());
            }
        }
    }

    Ok(())
}

async fn try_submit_block(
    template: &dinero_sv2_common::NewTemplateDinero,
    share: &SubmitSharesDinero,
    coinbase_full_hex: &str,
    mempool_tx_data: &[Vec<u8>],
    backends: &BackendPool,
) -> Result<SubmitBlockResult> {
    let block_hex = block::assemble_block_hex(template, share, coinbase_full_hex, mempool_tx_data)?;
    backends.submit_block(&block_hex).await
}

// =====================================================================
// Task 6: shared-mode (PPLNS) standard-share handling. Validates
// against the pool-owned `SharedTemplate` instead of the daemon
// template, credits the PPLNS window + journal on acceptance, and on
// block-worthy shares submits the pool-assembled shared coinbase.
// =====================================================================

#[allow(clippy::too_many_arguments)]
async fn handle_shared_share(
    session: &mut NoiseSession<TcpStream>,
    payload: &[u8],
    current: Option<&Arc<TemplateBundle>>,
    current_shared: Option<&Arc<SharedTemplate>>,
    share_target: [u8; 32],
    channel_id: u32,
    last_sequence_number: &mut u32,
    accepted_in_window: &mut u64,
    payout_script: &[u8],
    backends: &BackendPool,
    ledger: &Ledger,
    window: &Arc<Mutex<PplnsWindow>>,
    journal: &Arc<Mutex<WindowJournal>>,
    dedup: &Arc<Mutex<ShareDedup>>,
) -> Result<()> {
    let miner_key = miner_key_for_payout_script(payout_script);

    let share = match decode_submit_shares(payload) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "bad shared share shape");
            ledger.reject(miner_key);
            send_share_error(
                session,
                channel_id,
                *last_sequence_number,
                "invalid-payload",
            )
            .await?;
            return Ok(());
        }
    };
    *last_sequence_number = share.sequence_number;

    let Some(bundle) = current else {
        warn!("shared share received before any template");
        ledger.reject(miner_key);
        send_share_error(session, channel_id, share.sequence_number, "no-template").await?;
        return Ok(());
    };
    let Some(st) = current_shared else {
        warn!("shared share received but no shared template built this refresh");
        ledger.reject(miner_key);
        send_share_error(
            session,
            channel_id,
            share.sequence_number,
            "no-shared-template",
        )
        .await?;
        return Ok(());
    };

    if !job_generation::is_current_job(share.job_id, st.wire.template_id) {
        ledger.reject(miner_key);
        send_share_error(session, channel_id, share.sequence_number, "stale-share").await?;
        return Ok(());
    }

    let hash = HeaderAssembly::hash(&st.wire, &share);
    let meets_share = hash_meets_target(&hash, &share_target);
    // The shared template's nbits/difficulty is inherited verbatim from
    // the daemon template (`build_shared_template` only replaces
    // merkle_root/utreexo_root), so the daemon-derived block_target on
    // `bundle.pt` still applies here.
    let meets_block = hash_meets_target(&hash, &bundle.pt.block_target);

    if !meets_share {
        debug!(hash = %hex::encode(hash), "shared share below share-target");
        send_share_error(session, channel_id, share.sequence_number, "under-target").await?;
        return Ok(());
    }

    // Pool-wide dedup: never credit the same header hash twice —
    // whether resubmitted on this channel or found by another one.
    if !dedup.lock().expect("share dedup mutex").insert(hash) {
        warn!(
            hash = %hex::encode(hash),
            channel_id,
            payout = %hex::encode(payout_script),
            "duplicate shared share rejected"
        );
        ledger.reject(miner_key);
        send_share_error(
            session,
            channel_id,
            share.sequence_number,
            "duplicate-share",
        )
        .await?;
        return Ok(());
    }

    ledger.credit_share(miner_key);
    ops::telemetry().record_accepted_share("shared", hex::encode(hash));
    *accepted_in_window += 1;

    // Credit the PPLNS window + journal (amendment 5: SystemTime::now()
    // is fine in the pool binary).
    let weight = share_weight(&share_target);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    {
        let mut w = window.lock().expect("pplns window mutex");
        w.record(payout_script.to_vec(), weight, ts);
    }
    {
        // Separate lock scope from `window` above: `journal.compact`
        // used to take its own fresh `window` lock internally, so holding
        // the first guard across this block would deadlock (std::sync::
        // Mutex is not reentrant). It now takes a snapshot of entries
        // instead (see below), so the window lock is only ever held
        // briefly to clone the snapshot — never across the compact I/O
        // (serialize + flush + rename of up to 50k entries), which would
        // otherwise stall the template producer's own window lock and
        // block SOLO job production.
        let mut j = journal.lock().expect("pplns journal mutex");
        if let Err(e) = j.append(&WindowEntry {
            payout_script: payout_script.to_vec(),
            weight,
            unix_ts: ts,
        }) {
            warn!(error = %e, "pplns journal append failed — window credit still live in memory");
        }
        if j.should_compact() {
            let entries: Vec<WindowEntry> = {
                let w = window.lock().expect("pplns window mutex");
                w.entries().cloned().collect()
            };
            if let Err(e) = j.compact(&entries) {
                warn!(error = %e, "pplns journal compact failed");
            }
        }
    }

    info!(
        hash = %hex::encode(hash),
        template_id = st.wire.template_id,
        nonce = share.nonce,
        payout = %hex::encode(payout_script),
        "accepted shared share"
    );
    session
        .write_frame(
            MSG_SUBMIT_SHARES_SUCCESS,
            &encode_submit_shares_success(&SubmitSharesSuccess {
                channel_id,
                last_sequence_number: share.sequence_number,
                new_submits_accepted_count: 1,
                new_shares_sum: 1,
            }),
        )
        .await?;

    if meets_block {
        match try_submit_block(&st.wire, &share, &st.coinbase_full_hex, &[], backends).await {
            Ok(SubmitBlockResult::Accepted) => {
                info!(
                    template_id = st.wire.template_id,
                    hash = %hex::encode(hash),
                    contributors = st.outputs.len(),
                    outputs = ?st.outputs.iter()
                        .map(|o| format!("{}:{}", hex::encode(&o.script_pubkey), o.value_una))
                        .collect::<Vec<_>>(),
                    "★ SHARED block ACCEPTED — split across contributors"
                );
                ledger.credit_block(miner_key);
                ops::telemetry().record_block("accepted", hex::encode(hash), String::new());
            }
            Ok(SubmitBlockResult::Rejected(reason)) => {
                warn!(
                    reason,
                    hash = %hex::encode(hash),
                    "dinerod rejected our shared block"
                );
                ops::telemetry().record_block("rejected", hex::encode(hash), reason);
            }
            Err(e) => {
                warn!(error = %e, "submitblock RPC failed (shared)");
                ops::telemetry().record_block("error", hex::encode(hash), e.to_string());
            }
        }
    }

    Ok(())
}

// =====================================================================
// Phase 5: extended-share handling (miner supplies its own coinbase)
// =====================================================================

#[allow(clippy::too_many_arguments)]
async fn handle_extended_share(
    session: &mut NoiseSession<TcpStream>,
    payload: &[u8],
    current: Option<&PoolTemplate>,
    share_target: [u8; 32],
    channel_id: u32,
    last_sequence_number: &mut u32,
    accepted_in_window: &mut u64,
    miner_key: MinerKey,
    backends: &BackendPool,
    ledger: &Ledger,
    dedup: &Arc<Mutex<ShareDedup>>,
    utreexo_maturity_leaf_height: u32,
) -> Result<()> {
    let ext = match decode_submit_shares_extended(payload) {
        Ok(s) => s,
        Err(e) => {
            warn!(error = %e, "bad extended share shape");
            ledger.reject(miner_key);
            ops::telemetry().record_rejection("invalid-payload");
            let err = SubmitSharesError {
                channel_id,
                sequence_number: *last_sequence_number,
                error_code: b"invalid-payload".to_vec(),
            };
            session
                .write_frame(MSG_SUBMIT_SHARES_ERROR, &encode_submit_shares_error(&err)?)
                .await?;
            return Ok(());
        }
    };
    *last_sequence_number = ext.sequence_number;

    let Some(pt) = current else {
        warn!("extended share before any template");
        ledger.reject(miner_key);
        send_share_error(session, channel_id, ext.sequence_number, "no-template").await?;
        return Ok(());
    };
    if !job_generation::is_current_job(ext.job_id, pt.wire.template_id) {
        ledger.reject(miner_key);
        send_share_error(session, channel_id, ext.sequence_number, "stale-share").await?;
        return Ok(());
    }
    let Some(pre_block_state) = pt.utreexo_pre_block.as_ref() else {
        warn!("extended share but no pre-block Utreexo state");
        ledger.reject(miner_key);
        send_share_error(session, channel_id, ext.sequence_number, "no-utreexo-state").await?;
        return Ok(());
    };

    // 1. Validate the output value sum matches the block's coinbase value.
    let miner_total: u64 = ext.coinbase_outputs.iter().map(|o| o.value_una).sum();
    if miner_total != pt.coinbase_value_una {
        warn!(
            miner_total,
            expected = pt.coinbase_value_una,
            "extended share: coinbase output sum mismatch"
        );
        ledger.reject(miner_key);
        send_share_error(session, channel_id, ext.sequence_number, "value-mismatch").await?;
        return Ok(());
    }

    // 1b. Past the DNRF activation height, the coinbase MUST contain at
    //     least one OP_RETURN output with the DNRF commitment shape.
    //     Without this, dinerod rejects the block at submitblock and a
    //     found block is burned. We only validate the script SHAPE here
    //     (39 bytes, "DNRF" magic, version 0x01); dinerod re-verifies the
    //     filter_hash payload against the block's actual filter at
    //     accept-time. A buggy or stale miner client sending zero DNRF
    //     outputs is exactly the failure mode this guards.
    if requires_filter_commitment(pt.height as u64)
        && !ext
            .coinbase_outputs
            .iter()
            .any(|o| is_dnrf_script(&o.script_pubkey))
    {
        warn!(
            height = pt.height,
            outputs = ext.coinbase_outputs.len(),
            "extended share: missing DNRF commitment in miner outputs"
        );
        ledger.reject(miner_key);
        send_share_error(session, channel_id, ext.sequence_number, "missing-dnrf").await?;
        return Ok(());
    }

    // 1c. Past the witness-commitment mandatory height, the coinbase
    //     MUST contain the DNRW commitment with the exact value for
    //     this block's witness merkle root (pool blocks always carry
    //     the segwit reserved witness, so dinerod enforces it).
    //     Because the coinbase wtxid is zeros by convention, the
    //     expected script depends only on the template's mempool
    //     wtxids — the pool can compute it exactly and compare bytes.
    //     Without this, dinerod rejects the found block at ConnectTip
    //     (missing-witness-commitment / bad-witness-commitment).
    if requires_witness_commitment(pt.height as u64) {
        let wtxids: Vec<[u8; 32]> = pt
            .mempool_txs
            .iter()
            .map(|t| wtxid_from_tx_bytes(&t.data))
            .collect();
        let expected_dnrw = build_dnrw_script(&witness_merkle_root(&wtxids));
        if !ext
            .coinbase_outputs
            .iter()
            .any(|o| o.script_pubkey == expected_dnrw)
        {
            warn!(
                height = pt.height,
                outputs = ext.coinbase_outputs.len(),
                has_dnrw_shape = ext
                    .coinbase_outputs
                    .iter()
                    .any(|o| is_dnrw_script(&o.script_pubkey)),
                "extended share: missing or wrong DNRW witness commitment in miner outputs"
            );
            ledger.reject(miner_key);
            send_share_error(session, channel_id, ext.sequence_number, "missing-dnrw").await?;
            return Ok(());
        }
    }

    // 2. Reassemble the stripped coinbase using pool's prefix/suffix
    //    and the miner's outputs.
    let miner_outputs: Vec<CoinbaseOutput> = ext
        .coinbase_outputs
        .iter()
        .map(|w| CoinbaseOutput {
            value_una: w.value_una,
            script_pubkey: w.script_pubkey.clone(),
        })
        .collect();
    let (coinbase_stripped, coinbase_txid) =
        assemble_stripped_coinbase(&pt.coinbase_prefix, &miner_outputs, &pt.coinbase_suffix);

    // 3. Compute Utreexo leaf hashes for each output and apply.
    let mut post_state = pre_block_state.clone();
    for (i, out) in miner_outputs.iter().enumerate() {
        let leaf = leaf_hash_for_height(
            &coinbase_txid,
            i as u32,
            out.value_una,
            &out.script_pubkey,
            pt.height,
            true,
            utreexo_maturity_leaf_height,
        );
        if let Err(e) = post_state.add_leaf(leaf) {
            warn!(error = %e, "utreexo add_leaf failed");
            ledger.reject(miner_key);
            send_share_error(session, channel_id, ext.sequence_number, "utreexo-apply").await?;
            return Ok(());
        }
    }
    let utreexo_root = match utreexo_commitment(&post_state) {
        Ok(h) => h,
        Err(e) => {
            warn!(error = %e, "utreexo commitment failed");
            ledger.reject(miner_key);
            send_share_error(session, channel_id, ext.sequence_number, "utreexo-commit").await?;
            return Ok(());
        }
    };

    // 4. Merkle root from coinbase txid + (possibly empty) merkle_path.
    let merkle_root = compute_root(coinbase_txid, &pt.merkle_path);

    // 5. Reconstruct the header via `HeaderAssembly` using our
    //    computed (merkle_root, utreexo_root) and miner's (nonce,
    //    ntime, version). Everything else inherits from the job.
    let reconstructed = NewTemplateDinero {
        template_id: pt.wire.template_id,
        future_template: false,
        version: ext.version,
        prev_block_hash: pt.wire.prev_block_hash,
        merkle_root,
        utreexo_root,
        timestamp: ext.timestamp,
        difficulty: pt.wire.difficulty,
        coinbase_outputs_commitment: [0u8; 32], // not header-relevant
    };
    let share = SubmitSharesDinero {
        channel_id: ext.channel_id,
        sequence_number: ext.sequence_number,
        job_id: ext.job_id,
        nonce: ext.nonce,
        timestamp: ext.timestamp,
        version: ext.version,
    };
    let hash = HeaderAssembly::hash(&reconstructed, &share);
    let meets_share = hash_meets_target(&hash, &share_target);
    let meets_block = hash_meets_target(&hash, &pt.block_target);

    if !meets_share {
        debug!(hash = %hex::encode(hash), "extended share below share-target");
        send_share_error(session, channel_id, ext.sequence_number, "under-target").await?;
        return Ok(());
    }

    // Pool-wide dedup: extended-share miners own their coinbase (work
    // is already unique per miner), but resubmitting the same share
    // must still not double-credit.
    if !dedup.lock().expect("share dedup mutex").insert(hash) {
        warn!(hash = %hex::encode(hash), channel_id, "duplicate extended share rejected");
        ledger.reject(miner_key);
        send_share_error(session, channel_id, ext.sequence_number, "duplicate-share").await?;
        return Ok(());
    }

    ledger.credit_share(miner_key);
    ops::telemetry().record_accepted_share("extended", hex::encode(hash));
    *accepted_in_window += 1;
    info!(
        hash = %hex::encode(hash),
        template_id = pt.wire.template_id,
        nonce = ext.nonce,
        utreexo_root = %hex::encode(utreexo_root),
        "accepted extended share"
    );
    session
        .write_frame(
            MSG_SUBMIT_SHARES_SUCCESS,
            &encode_submit_shares_success(&SubmitSharesSuccess {
                channel_id,
                last_sequence_number: ext.sequence_number,
                new_submits_accepted_count: 1,
                new_shares_sum: 1,
            }),
        )
        .await?;

    if meets_block {
        // Reassemble the full block (segwit coinbase) for submitblock:
        //   stripped-coinbase bytes with segwit marker+flag inserted
        //   after version, and the pool-retained witness bytes inserted
        //   before the locktime. Then the mempool tx bytes follow.
        let full_coinbase = block::wrap_stripped_with_segwit_witness(
            &coinbase_stripped,
            &pt.coinbase_witness_bytes,
            &pt.coinbase_suffix,
        );
        let mempool_data: Vec<Vec<u8>> = pt.mempool_txs.iter().map(|t| t.data.clone()).collect();
        match block::assemble_block_hex_raw(&reconstructed, &share, &full_coinbase, &mempool_data) {
            Ok(block_hex) => match backends.submit_block(&block_hex).await {
                Ok(SubmitBlockResult::Accepted) => {
                    info!("★ extended-share block ACCEPTED by dinerod");
                    ledger.credit_block(miner_key);
                    ops::telemetry().record_block("accepted", hex::encode(hash), String::new());
                }
                Ok(SubmitBlockResult::Rejected(reason)) => {
                    warn!(reason, "dinerod rejected our extended-share block");
                    ops::telemetry().record_block("rejected", hex::encode(hash), reason);
                }
                Err(e) => {
                    ops::telemetry().record_block("error", hex::encode(hash), e.to_string());
                    warn!(error = %e, "submitblock RPC failed")
                }
            },
            Err(e) => warn!(error = %e, "assemble_block_hex_raw failed"),
        }
    }

    Ok(())
}

async fn send_share_error(
    session: &mut NoiseSession<TcpStream>,
    channel_id: u32,
    sequence_number: u32,
    code: &str,
) -> Result<()> {
    ops::telemetry().record_rejection(code);
    let err = SubmitSharesError {
        channel_id,
        sequence_number,
        error_code: code.as_bytes().to_vec(),
    };
    session
        .write_frame(MSG_SUBMIT_SHARES_ERROR, &encode_submit_shares_error(&err)?)
        .await?;
    Ok(())
}

/// Count leading zero bits in a 32-byte big-endian target. Used to
/// estimate the miner's effective hashrate from observed share count
/// under the current target shape (which is always `0..0 1..1` from
/// `leading_zero_bits_target`). For a non-leading-zero-shape target
/// this is just an approximation, but the vardiff loop only ever
/// produces leading-zero-shape targets so the cycle is consistent.
fn count_leading_zero_bits(target: &[u8; 32]) -> u32 {
    let mut bits = 0u32;
    for byte in target {
        if *byte == 0 {
            bits += 8;
        } else {
            bits += byte.leading_zeros();
            break;
        }
    }
    bits
}

/// Apply mempool tx deletions (inputs) and additions (outputs) to the
/// chain-tip pre-block Utreexo state, producing the pre-coinbase state
/// that JD miners build their own coinbase on top of.
///
/// Pulls inclusion proofs for each input via `getutxoproofs_batch`, then
/// applies them via `UtreexoAccumulatorState::apply_deletions`, then
/// adds each tx's outputs as new leaves via `add_leaf` (Utreexo
/// additions are O(log n) per leaf and don't need any RPC).
///
/// Errors propagate to the caller, which falls back to a coinbase-only
/// template on failure (better an empty block than a wrong utreexo
/// commitment).
async fn apply_mempool_to_pre_coinbase(
    rpc: &rpc::RpcClient,
    pre_block: &dinero_sv2_jd::UtreexoAccumulatorState,
    mempool_txs: &[mapper::MempoolTx],
    block_height: u32,
    utreexo_maturity_leaf_height: u32,
) -> Result<dinero_sv2_jd::UtreexoAccumulatorState> {
    use dinero_sv2_jd::DeletionTarget;

    // Collect all inputs across all mempool txs. Daemon RPC takes
    // display-order txid hex.
    let mut outpoints: Vec<(String, u32)> = Vec::new();
    for tx in mempool_txs {
        for (prev_raw, vout) in &tx.inputs {
            let mut display = *prev_raw;
            display.reverse();
            outpoints.push((hex::encode(display), *vout));
        }
    }

    // Fetch proofs.
    let mut deletions: Vec<DeletionTarget> = Vec::with_capacity(outpoints.len());
    if !outpoints.is_empty() {
        let resp = rpc
            .get_utxo_proofs_batch(&outpoints)
            .await
            .context("getutxoproofs_batch RPC")?;
        let proofs = resp
            .get("proofs")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow::anyhow!("getutxoproofs_batch: missing proofs[]"))?;
        if proofs.len() != outpoints.len() {
            anyhow::bail!(
                "getutxoproofs_batch returned {} entries for {} requested outpoints",
                proofs.len(),
                outpoints.len()
            );
        }
        for (i, p) in proofs.iter().enumerate() {
            let success = p.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
            if !success {
                let why = p.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
                anyhow::bail!(
                    "getutxoproofs_batch: outpoint #{i} ({}:{}) failed: {why}",
                    outpoints[i].0,
                    outpoints[i].1
                );
            }
            let leaf_hex = p
                .get("leaf_hash")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("proof #{i}: missing leaf_hash"))?;
            let position = p
                .get("position")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| anyhow::anyhow!("proof #{i}: missing position"))?;
            let siblings_arr = p
                .get("siblings")
                .and_then(|v| v.as_array())
                .ok_or_else(|| anyhow::anyhow!("proof #{i}: missing siblings"))?;
            let leaf_bytes =
                hex::decode(leaf_hex).with_context(|| format!("proof #{i} leaf_hash hex"))?;
            if leaf_bytes.len() != 32 {
                anyhow::bail!("proof #{i}: leaf_hash is {} bytes", leaf_bytes.len());
            }
            let mut leaf_hash_arr = [0u8; 32];
            leaf_hash_arr.copy_from_slice(&leaf_bytes);
            let mut siblings: Vec<[u8; 32]> = Vec::with_capacity(siblings_arr.len());
            for (j, s) in siblings_arr.iter().enumerate() {
                let s_hex = s
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("proof #{i} sibling[{j}] not a string"))?;
                let sb =
                    hex::decode(s_hex).with_context(|| format!("proof #{i} sibling[{j}] hex"))?;
                if sb.len() != 32 {
                    anyhow::bail!("proof #{i} sibling[{j}] is {} bytes", sb.len());
                }
                let mut a = [0u8; 32];
                a.copy_from_slice(&sb);
                siblings.push(a);
            }
            deletions.push(DeletionTarget {
                position,
                leaf_hash: leaf_hash_arr,
                siblings,
            });
        }
    }

    // Apply deletions, then per-tx output additions.
    let mut state = pre_block.clone();
    state
        .apply_deletions(&deletions)
        .context("utreexo apply_deletions")?;
    for tx in mempool_txs {
        for (vout, (value_una, spk)) in tx.outputs.iter().enumerate() {
            let leaf = leaf_hash_for_height(
                &tx.txid_raw,
                vout as u32,
                *value_una,
                spk,
                block_height,
                false,
                utreexo_maturity_leaf_height,
            );
            state.add_leaf(leaf).context("utreexo add_leaf")?;
        }
    }
    Ok(state)
}

#[cfg(test)]
mod cli_tests {
    use super::Args;
    use clap::Parser;

    // The installer calls `--print-pubkey` on its own to show operators the key
    // their miners must pin. When clap demanded --payout-address here, that
    // call exited 2 with empty stdout and the banner printed a placeholder.
    #[test]
    fn print_pubkey_does_not_require_a_payout_address() {
        let a = Args::try_parse_from(["pool", "--print-pubkey", "--tp-key", "/tmp/k"])
            .expect("--print-pubkey must stand alone");
        assert!(a.print_pubkey);
        assert!(a.payout_address.is_none());
    }

    // ...but running the pool without one must still be refused, not defaulted.
    #[test]
    fn running_the_pool_still_requires_a_payout_address() {
        assert!(Args::try_parse_from(["pool", "--bind", "127.0.0.1:4444"]).is_err());
    }

    #[test]
    fn payout_change_is_off_unless_asked_for() {
        let a = Args::try_parse_from(["pool", "--payout-address", "din1pxx"]).unwrap();
        assert!(!a.ops_allow_payout_change, "must default OFF");
        let b = Args::try_parse_from([
            "pool",
            "--payout-address",
            "din1pxx",
            "--ops-allow-payout-change",
        ])
        .unwrap();
        assert!(b.ops_allow_payout_change);
    }

    #[test]
    fn fee_change_is_off_unless_asked_for() {
        let a = Args::try_parse_from(["pool", "--payout-address", "din1pxx"]).unwrap();
        assert!(!a.ops_allow_fee_change, "must default OFF");
        let b = Args::try_parse_from([
            "pool",
            "--payout-address",
            "din1pxx",
            "--ops-allow-fee-change",
        ])
        .unwrap();
        assert!(b.ops_allow_fee_change);
    }
}
