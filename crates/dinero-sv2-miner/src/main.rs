//! Dinero Stratum V2 CPU miner.
//!
//! Long-running client that speaks SV2 + Noise NX to a pool like the one on
//! LA. Receives `NewMiningJob` frames, hashes a rayon-parallel nonce sweep
//! against the channel's share target, and submits `SubmitSharesExtended`
//! with miner-owned coinbase outputs (Job Declaration path).
//!
//! Designed to be spawned from a GUI wrapper (dinero-qt) with structured
//! event output via `--json`, but also usable standalone from a terminal.

use anyhow::{bail, Context, Result};
use clap::{Parser, ValueEnum};
use dinero_sv2_codec::sv2::{decode_window_status, encode_set_reward_mode};
use dinero_sv2_codec::{
    decode_coinbase_context, decode_new_template, decode_open_standard_mining_channel_success,
    decode_set_new_prev_hash, decode_set_target, decode_setup_connection_success,
    decode_submit_shares_error, decode_submit_shares_success, encode_open_standard_mining_channel,
    encode_setup_connection, encode_submit_shares, encode_submit_shares_extended,
};
use dinero_sv2_common::{
    nbits_to_target, CoinbaseContext, CoinbaseOutputWire, HeaderAssembly, NewTemplateDinero,
    OpenStandardMiningChannel, SetRewardMode, SetupConnection, SubmitSharesDinero,
    SubmitSharesExtendedDinero, PROTOCOL_MINING, PROTOCOL_VERSION,
};
use dinero_sv2_jd::{
    assemble_stripped_coinbase,
    block_filter::{gcs_build, gcs_filter_hash},
    commitment as utreexo_commitment, compute_root, decode_utreexo_accumulator_state,
    filter_commitment::{build_dnrf_script, requires_filter_commitment},
    leaf_hash_for_height,
    witness_commitment::{build_dnrw_script_coinbase_only, requires_witness_commitment},
    CoinbaseOutput, UtreexoAccumulatorState, UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET,
};
use dinero_sv2_transport::{
    Frame, NoiseReader, NoiseSession, MSG_COINBASE_CONTEXT, MSG_NEW_MINING_JOB,
    MSG_OPEN_STANDARD_MINING_CHANNEL, MSG_OPEN_STANDARD_MINING_CHANNEL_ERROR,
    MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS, MSG_SETUP_CONNECTION, MSG_SETUP_CONNECTION_ERROR,
    MSG_SETUP_CONNECTION_SUCCESS, MSG_SET_NEW_PREV_HASH, MSG_SET_REWARD_MODE, MSG_SET_TARGET,
    MSG_SUBMIT_SHARES_ERROR, MSG_SUBMIT_SHARES_EXTENDED, MSG_SUBMIT_SHARES_STANDARD,
    MSG_SUBMIT_SHARES_SUCCESS, MSG_UTREEXO_STATE, MSG_WINDOW_STATUS,
};
use dinero_miner_ux::config::FileConfig;
use dinero_miner_ux::display::{Display, SessionStats};
use rayon::prelude::*;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::sleep;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum RewardModeChoice {
    Solo,
    Shared,
}

impl RewardModeChoice {
    fn as_str(self) -> &'static str {
        match self {
            Self::Solo => "solo",
            Self::Shared => "shared",
        }
    }
}

#[derive(Parser, Clone)]
#[command(version, about = "Dinero SV2 pool miner")]
struct Args {
    /// Pool endpoint as host:port, e.g. pool.dinerolabs.org:4444 or an
    /// ip:port. Defaults to the well-known Dinero pool when not set here
    /// or in the saved config. Hostnames re-resolve on every reconnect.
    #[arg(long)]
    pool: Option<String>,

    /// Expected pool static pubkey (64-char hex). Defaults to the well-known
    /// Dinero pool's pubkey when not set here or in the saved config.
    #[arg(long)]
    server_pubkey: Option<String>,

    /// Dinero payout address (din1p…). Prompted for interactively if
    /// omitted, stdin is a terminal, and no address is saved yet.
    #[arg(long, conflicts_with = "payout_script_hex")]
    address: Option<String>,

    /// Coinbase payout scriptPubKey as hex (34 bytes for Taproot `din1p…`).
    /// Consensus sends the block reward to this script on a block-find.
    /// Legacy alternative to --address; bypasses address validation.
    #[arg(long)]
    payout_script_hex: Option<String>,

    /// Reward ownership: solo uses a miner-owned coinbase; shared submits
    /// standard shares to the pool's PPLNS window. Unspecified resolves to
    /// shared; pass this flag (or set it in the saved config) to mine solo.
    #[arg(long, value_enum)]
    reward_mode: Option<RewardModeChoice>,

    /// Worker identity reported to the pool. Shows up in pool logs.
    #[arg(long, default_value = "dinero-sv2-miner")]
    user_agent: String,

    /// Number of CPU hash threads. Unspecified resolves to cores-1 (or the
    /// saved config's value). Explicit 0 = detect and use all logical cores.
    #[arg(long)]
    threads: Option<usize>,

    /// Emit newline-delimited JSON events on stdout instead of human-
    /// readable lines. Intended for GUI frontends to parse.
    #[arg(long)]
    json: bool,

    /// Skip writing the resolved address/pool/pubkey/mode/threads to the
    /// config file. Useful for tests and scripted one-off runs.
    #[arg(long)]
    no_save: bool,

    /// Reconnect back-off seconds on disconnect. 0 = exit on disconnect.
    #[arg(long, default_value_t = 5)]
    reconnect_secs: u64,

    /// Stop after this many block-target solutions (not share-target).
    /// 0 = run forever. Mostly for testing.
    #[arg(long, default_value_t = 0)]
    max_blocks: u64,

    /// Quiet human display (the pre-FX single status line). FX (live hash
    /// feed + color) is the default on interactive terminals.
    #[arg(long)]
    plain: bool,
}

/// Which output mode `async_main` should construct, resolved from
/// `--json`/TTY-ness/`--plain`/terminal-FX-support. Kept as a pure function
/// of those four booleans so the decision table is unit-testable without a
/// real terminal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ModeChoice {
    Json,
    PlainMachine,
    HumanV1,
    Fx,
}

/// `json` wins outright (GUI wire format). Otherwise: a non-TTY stdout is
/// always the machine-readable plain-text path (log redirection etc.).
/// On a TTY, `--plain` (or a terminal that doesn't support the FX escape
/// sequences) falls back to the v1 single-status-line human display;
/// otherwise FX is the default.
fn choose_mode(json: bool, tty: bool, plain: bool, term_ok: bool) -> ModeChoice {
    if json {
        ModeChoice::Json
    } else if !tty {
        ModeChoice::PlainMachine
    } else if plain || !term_ok {
        ModeChoice::HumanV1
    } else {
        ModeChoice::Fx
    }
}

fn main() -> Result<()> {
    // Use a multi-threaded tokio runtime so share submits don't block
    // the reader.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let args = Args::parse();

    // ---- Config precedence: flags > saved file > built-in defaults ----
    let flags = args_to_flags(&args);
    let config_path = dinero_miner_ux::config::config_path();
    let file = dinero_miner_ux::config::load(&config_path);
    let cores = num_cpus::get();
    let mut effective = dinero_miner_ux::config::resolve(&flags, &file, cores);
    if args.threads == Some(0) {
        // Explicit `--threads 0` keeps its legacy meaning (all logical
        // cores) rather than the resolver's cores-1 default; it never
        // touches the saved config's threads value either.
        effective.threads = cores;
    }
    // Floor at 1: a hand-edited (or pre-fix) config with `"threads": 0`
    // must never silently produce an empty rayon range split — that would
    // connect, accept jobs, and hash nothing.
    let threads = effective.threads.max(1);
    let reward_mode = resolve_reward_mode(&effective.reward_mode);

    // Rayon global pool sized to the chosen thread count. `build_global`
    // fails if called twice in the same process — fine here since we only
    // ever call it once.
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .ok();

    let mode = choose_mode(
        args.json,
        std::io::stdout().is_terminal(),
        args.plain,
        dinero_miner_ux::theme::term_supports_fx(),
    );
    // FX mode shares the v1 prompt look (banner-lite) — only the emitter's
    // steady-state rendering differs.
    let human = matches!(mode, ModeChoice::HumanV1 | ModeChoice::Fx);

    // ---- Resolve the payout address / script ----
    // `--payout-script-hex` is a legacy escape hatch: verbatim, no address
    // validation. Otherwise resolve/prompt for a `din1p…` address and derive
    // the script from it. A resolved address that fails validation is only
    // recoverable (warn + fall back to the prompt, or fail naming the
    // config path) when it came from the saved file, not from `--address`
    // — a flag-sourced bad address keeps failing fast, unchanged.
    let (resolved_address, payout_script): (Option<String>, Vec<u8>) =
        if let Some(hex_script) = args.payout_script_hex.as_deref() {
            let script = hex::decode(hex_script).context("payout_script_hex must be hex")?;
            (effective.address.clone(), script)
        } else if let Some(addr) = effective.address.clone() {
            match classify_resolved_address(args.address.as_deref(), &addr) {
                AddressOutcome::Ready(script) => (Some(addr), script),
                AddressOutcome::Fatal(err) => return Err(err),
                AddressOutcome::Recoverable(err) => {
                    if std::io::stdin().is_terminal() {
                        eprintln!(
                            "warning: saved address in {} is invalid ({err}) — ignoring it",
                            config_path.display()
                        );
                        prompt_for_new_address(human)?
                    } else {
                        bail!(
                            "saved address in {} is invalid: {err} (pass --address/--payout-script-hex, or run interactively to replace it)",
                            config_path.display()
                        );
                    }
                }
            }
        } else if std::io::stdin().is_terminal() {
            prompt_for_new_address(human)?
        } else {
            bail!(
                "no payout address: pass --address din1p… (or --payout-script-hex), or run interactively"
            );
        };

    if payout_script.is_empty() {
        bail!("payout_script_hex decoded to empty script");
    }
    validate_reward_payout(reward_mode, &payout_script)?;

    let pool = effective.pool.clone();
    validate_pool_endpoint(&pool)?;
    let pinned = parse_server_pubkey(effective.server_pubkey.as_deref())?;
    if pinned.is_none() {
        eprintln!(
            "warning: no server pubkey configured for pool {pool} — connection is unpinned (trust-on-first-use)"
        );
    }

    let reward_address = resolved_address.clone()
        .unwrap_or_else(|| "custom payout script".to_string());
    if !args.no_save {
        save_config(&config_path, &file, &args, resolved_address);
    }

    // Shared job snapshot + live nonce position for the FX display's
    // real-hash sampler. Harmless to build unconditionally (a few extra
    // words of Mutex/Atomic) — only ever populated/read in FX mode.
    let sampler_state = Arc::new(SamplerState {
        job: Mutex::new(None),
        nonce_hint: AtomicU64::new(0),
    });
    // Never set until process exit (the Ctrl-C path exits the process
    // directly) — reconnects reuse the same screen/ticker.
    let stop_flag = Arc::new(AtomicBool::new(false));

    let emitter = if matches!(mode, ModeChoice::Fx) {
        let fx = dinero_miner_ux::fx::FxScreen::new(
            Box::new(std::io::stdout()),
            dinero_miner_ux::fx::FxConfig {
                width: dinero_miner_ux::theme::term_width(),
                colors: dinero_miner_ux::theme::colors_enabled(),
                reward_mode: reward_mode.as_str().to_string(),
                frame_delay_ms: 160,
                pool: pool.clone(),
                threads,
                pinned: pinned.is_some(),
                reward_address,
            },
        );

        // Establish the alternate screen and permanent logo before the ticker
        // can paint its first dashboard frame. Reversing these two operations
        // creates a race where the cursor origin is captured above the logo.
        fx.print_banner();

        // Build the real-hash sampler and spawn the ticker below the logo.
        let sampler_state2 = Arc::clone(&sampler_state);
        let sampler: dinero_miner_ux::fx::HashSampler = Arc::new(move || {
            let job = sampler_state2.job.lock().ok()?.clone()?;
            let hint = sampler_state2.nonce_hint.load(Ordering::Relaxed);
            let share = SubmitSharesDinero {
                channel_id: 0,
                sequence_number: 0,
                job_id: 0,
                nonce: hint as u32,
                timestamp: job.timestamp + (hint >> 32),
                version: job.version,
            };
            Some(dinero_miner_ux::fx::CandidateSample {
                nonce: hint as u32,
                hash: HeaderAssembly::hash(&job, &share),
            })
        });
        fx.spawn_ticker(sampler, Arc::clone(&stop_flag));
        Emitter::new_fx(fx)
    } else {
        Emitter::new(args.json, human)
    };
    emitter.spawn_ctrlc_summary_handler();
    emitter.emit_startup(&pool, pinned.is_some(), threads, &args.user_agent);

    // Process-wide generation counter. Lives across reconnects so that
    // hashing/telemetry threads spawned in a previous session observe a
    // bump when the next session starts and exit cleanly. Without this,
    // a fresh `generation` Arc per session left old workers hashing
    // forever (especially after the timestamp-wrap fix made them
    // immortal within their range), each spawning their own telemetry
    // thread that emitted spurious mhs:0 alongside the real ones.
    let generation = Arc::new(AtomicU64::new(0));

    // Process-wide rolling hashrate, updated by each telemetry thread.
    // Stored as MH/s × 100 (centi-MH/s) to keep an integer atomic; read
    // back at each reconnect so `nominal_hash_rate_bits` reports the
    // measured rate instead of a hardcoded `threads * 3 MH/s` lie.
    let measured_mhs_x100 = Arc::new(AtomicU64::new(0));

    let mut blocks_found: u64 = 0;
    loop {
        match run_session(
            &args,
            &pool,
            reward_mode,
            pinned.as_ref(),
            &payout_script,
            threads,
            Arc::clone(&generation),
            Arc::clone(&measured_mhs_x100),
            Arc::clone(&sampler_state),
            &emitter,
        )
        .await
        {
            Ok(round_blocks) => {
                blocks_found += round_blocks;
                if args.max_blocks > 0 && blocks_found >= args.max_blocks {
                    emitter.emit(
                        "session_end",
                        &serde_json::json!({
                            "reason": "max-blocks-reached",
                            "blocks_found": blocks_found,
                        }),
                    );
                    emitter.print_fx_summary();
                    return Ok(());
                }
                emitter.emit(
                    "session_end",
                    &serde_json::json!({"reason": "clean-close", "blocks_found": blocks_found}),
                );
            }
            Err(err) => {
                emitter.emit(
                    "session_end",
                    &serde_json::json!({
                        "reason": "error",
                        "error": err.to_string(),
                    }),
                );
                if args.reconnect_secs == 0 {
                    emitter.print_fx_summary();
                    return Err(err);
                }
            }
        }
        if args.reconnect_secs == 0 {
            emitter.print_fx_summary();
            return Ok(());
        }
        emitter.emit(
            "reconnect_wait",
            &serde_json::json!({"seconds": args.reconnect_secs}),
        );
        sleep(Duration::from_secs(args.reconnect_secs)).await;
    }
}

/// Maps CLI flags onto the config layer's `FileConfig` shape. Only ever
/// `Some` for a field the user actually supplied — that's what lets the
/// resolver's flag > file > default precedence work.
fn args_to_flags(args: &Args) -> FileConfig {
    FileConfig {
        address: args.address.clone(),
        pool: args.pool.clone(),
        server_pubkey: args.server_pubkey.clone(),
        reward_mode: args.reward_mode.map(|m| m.as_str().to_string()),
        threads: match args.threads {
            // Legacy "all cores" meaning is handled directly in async_main
            // by overriding the resolved thread count; don't let a literal
            // 0 flow into the resolver (it would be taken as "zero
            // threads").
            Some(0) => None,
            other => other,
        },
    }
}

/// Syntactic host:port check, done once at startup so garbage fails fast
/// with a clear message. Deliberately NOT a DNS resolution: name lookup
/// happens at connect time each session, and a temporarily-unresolvable
/// name goes through the normal reconnect/backoff path instead of
/// aborting startup.
fn validate_pool_endpoint(pool: &str) -> Result<()> {
    let valid = pool
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
    if !valid {
        bail!("invalid pool address '{pool}': expected host:port, e.g. pool.dinerolabs.org:4444");
    }
    Ok(())
}

/// Parses the resolved `reward_mode` string (case-insensitively). An
/// unrecognized value — e.g. a hand-edited config file — falls back to the
/// resolver's own default (shared) with a one-line warning instead of
/// failing startup.
fn resolve_reward_mode(s: &str) -> RewardModeChoice {
    match s.to_lowercase().as_str() {
        "solo" => RewardModeChoice::Solo,
        "shared" => RewardModeChoice::Shared,
        other => {
            eprintln!("warning: unknown reward_mode '{other}' in config; using shared");
            RewardModeChoice::Shared
        }
    }
}

/// How a resolved (flag-or-config) address held up under validation.
enum AddressOutcome {
    /// Valid: the derived 34-byte P2TR payout script.
    Ready(Vec<u8>),
    /// Invalid and flag-sourced: the user typed it just now, fail fast.
    Fatal(anyhow::Error),
    /// Invalid but config-sourced: stale/hand-edited saved file — the
    /// caller may fall back to the interactive prompt.
    Recoverable(anyhow::Error),
}

/// Validates the resolved address, classifying a failure by its source:
/// `flag_address` is `Some` when `--address` was given (in which case it is
/// what `addr` resolved from, since flags outrank the config file).
fn classify_resolved_address(flag_address: Option<&str>, addr: &str) -> AddressOutcome {
    match decode_address(addr) {
        Ok(script) => AddressOutcome::Ready(script),
        Err(err) if flag_address.is_some() => AddressOutcome::Fatal(err),
        Err(err) => AddressOutcome::Recoverable(err),
    }
}

/// Runs the interactive address prompt (no saved-address offer — callers
/// reach this only when there is no usable saved address) and derives the
/// payout script. A prompt abort exits the process with status 2.
fn prompt_for_new_address(human: bool) -> Result<(Option<String>, Vec<u8>)> {
    if human {
        eprintln!("⛏  Dinero SV2 Miner");
    }
    let mut stdin = std::io::BufReader::new(std::io::stdin());
    let mut stderr = std::io::stderr();
    match dinero_miner_ux::prompt::prompt_for_address(&mut stdin, &mut stderr, None) {
        dinero_miner_ux::prompt::PromptOutcome::Address(addr) => {
            let script = decode_address(&addr)?;
            Ok((Some(addr), script))
        }
        dinero_miner_ux::prompt::PromptOutcome::Aborted => std::process::exit(2),
    }
}

/// Validates a `din1p…` address and returns its 34-byte P2TR scriptPubKey
/// as raw bytes.
fn decode_address(addr: &str) -> Result<Vec<u8>> {
    let script_hex =
        dinero_miner_ux::address::payout_script_hex(addr).map_err(|e| anyhow::anyhow!(e.message()))?;
    hex::decode(script_hex).context("payout_script_hex produced non-hex output")
}

/// Persists the resolved address (always, when present) plus any
/// explicitly-flagged pool/pubkey/mode/threads, merged onto whatever was
/// already on disk so unrelated saved settings survive a run that didn't
/// touch them. Best-effort: a write failure is a warning, not a fatal error.
fn save_config(path: &std::path::Path, file: &FileConfig, args: &Args, resolved_address: Option<String>) {
    let mut to_save = file.clone();
    if let Some(addr) = resolved_address {
        to_save.address = Some(addr);
    }
    if args.pool.is_some() {
        to_save.pool = args.pool.clone();
    }
    if args.server_pubkey.is_some() {
        to_save.server_pubkey = args.server_pubkey.clone();
    }
    if let Some(mode) = args.reward_mode {
        to_save.reward_mode = Some(mode.as_str().to_string());
    }
    if let Some(threads) = args.threads {
        to_save.threads = if threads == 0 { None } else { Some(threads) };
    }
    if let Err(err) = dinero_miner_ux::config::save(path, &to_save) {
        eprintln!("warning: could not save config to {}: {err}", path.display());
    }
}

fn validate_reward_payout(mode: RewardModeChoice, payout_script: &[u8]) -> Result<()> {
    if mode == RewardModeChoice::Shared
        && (payout_script.len() != 34 || payout_script[0] != 0x51 || payout_script[1] != 0x20)
    {
        bail!("shared rewards require a Taproot payout address (34-byte P2TR script)");
    }
    Ok(())
}

fn parse_server_pubkey(hex_opt: Option<&str>) -> Result<Option<[u8; 32]>> {
    match hex_opt {
        Some(h) => {
            let bytes = hex::decode(h).context("server-pubkey must be hex")?;
            if bytes.len() != 32 {
                bail!("server-pubkey must be 32 bytes (64 hex chars)");
            }
            let mut out = [0u8; 32];
            out.copy_from_slice(&bytes);
            Ok(Some(out))
        }
        None => Ok(None),
    }
}

/// One full SV2 session. Returns the number of block-target hits found
/// before the session closed normally, or an error if the pool disconnected
/// unexpectedly.
#[allow(clippy::too_many_arguments)]
async fn run_session(
    args: &Args,
    pool: &str,
    reward_mode: RewardModeChoice,
    pinned: Option<&[u8; 32]>,
    payout_script: &[u8],
    threads: usize,
    generation: Arc<AtomicU64>,
    measured_mhs_x100: Arc<AtomicU64>,
    sampler_state: Arc<SamplerState>,
    emitter: &Emitter,
) -> Result<u64> {
    // `connect(&str)` resolves the hostname per attempt, so a pool DNS
    // repoint takes effect at the next reconnect without a restart.
    let tcp = TcpStream::connect(pool).await.context("connect")?;
    let session = NoiseSession::initiate_nx(tcp, pinned)
        .await
        .context("noise handshake")?;
    let peer_pubkey = hex::encode(session.peer_static_key());
    emitter.emit(
        "connected",
        &serde_json::json!({
            "pool": pool.to_string(),
            "server_pubkey": peer_pubkey,
            "threads": threads,
        }),
    );

    // Split into reader/writer halves. Reader runs in a dedicated task
    // so its in-flight `read_frame` future is never dropped by a
    // `select!` — dropping mid-read desyncs the Noise cipher and the
    // pool immediately disconnects with a decrypt error.
    let (mut reader, mut writer) = session.split();

    // ---- SetupConnection ----
    let setup = SetupConnection {
        protocol: PROTOCOL_MINING,
        min_version: PROTOCOL_VERSION,
        max_version: PROTOCOL_VERSION,
        flags: 0,
        user_agent: args.user_agent.as_bytes().to_vec(),
    };
    writer
        .write_frame(MSG_SETUP_CONNECTION, &encode_setup_connection(&setup)?)
        .await?;
    expect_setup_success(&mut reader).await?;

    // ---- OpenStandardMiningChannel ----
    // Report the rolling measured hashrate from the previous session.
    // First connect (atomic still 0) falls back to the ballpark guess so
    // pools have a non-zero value to size targets from. Subsequent
    // reconnects use the real number, which lets pool-side vardiff
    // (#11) calibrate share targets to actual capacity instead of a
    // hardcoded `threads * 3 MH/s` lie.
    let measured = measured_mhs_x100.load(Ordering::Relaxed);
    let nominal_hps = if measured > 0 {
        (measured as f32 / 100.0) * 1_000_000.0
    } else {
        (threads as f32) * 3_000_000.0
    };
    let open = OpenStandardMiningChannel {
        request_id: 1,
        user_identity: args.user_agent.as_bytes().to_vec(),
        nominal_hash_rate_bits: f32::to_bits(nominal_hps),
        max_target: [0xFFu8; 32],
    };
    writer
        .write_frame(
            MSG_OPEN_STANDARD_MINING_CHANNEL,
            &encode_open_standard_mining_channel(&open)?,
        )
        .await?;
    let (channel_id, mut share_target) = expect_channel_open(&mut reader).await?;
    emitter.emit(
        "channel_open",
        &serde_json::json!({
            "channel_id": channel_id,
            "share_target": hex::encode(share_target),
            "reward_mode": reward_mode.as_str(),
        }),
    );

    if reward_mode == RewardModeChoice::Shared {
        let declaration = SetRewardMode {
            channel_id,
            mode: 1,
            payout_script: payout_script.to_vec(),
        };
        writer
            .write_frame(MSG_SET_REWARD_MODE, &encode_set_reward_mode(&declaration)?)
            .await?;
    }

    // Move the reader into a task that forwards frames via channel.
    let (frame_tx, mut frame_rx) = mpsc::unbounded_channel::<Frame>();
    let reader_task = tokio::spawn(async move {
        loop {
            match reader.read_frame().await {
                Ok(Some(f)) => {
                    if frame_tx.send(f).is_err() {
                        return Ok(());
                    }
                }
                Ok(None) => return Ok(()),
                Err(e) => return Err(e),
            }
        }
    });

    // Session state carried across frames.
    let mut pre_block_state: Option<UtreexoAccumulatorState> = None;
    let mut coinbase_ctx: Option<CoinbaseContext> = None;
    let mut pending_shared_template: Option<NewTemplateDinero> = None;
    let mut shared_mode_confirmed = false;
    let mut blocks_found: u64 = 0;
    let mut seq: u32 = 0;

    let cancel = Arc::new(AtomicBool::new(false));
    let (share_tx, mut share_rx) = mpsc::unbounded_channel::<FoundShare>();

    let result: Result<u64> = loop {
        tokio::select! {
            frame = frame_rx.recv() => {
                let Some(frame) = frame else {
                    break Err(anyhow::anyhow!("pool closed the session"));
                };
                match frame.msg_type {
                    MSG_SET_NEW_PREV_HASH => {
                        let snph = decode_set_new_prev_hash(&frame.payload)?;
                        emitter.emit(
                            "set_new_prev_hash",
                            &serde_json::json!({
                                "prev_hash": hex::encode(snph.prev_hash),
                                "min_ntime": snph.min_ntime,
                                "nbits": format!("0x{:08x}", snph.nbits),
                            }),
                        );
                        cancel.store(true, Ordering::SeqCst);
                        // New prev hash invalidates pre-block state until
                        // the pool re-sends it with the next job cycle.
                        pre_block_state = None;
                        coinbase_ctx = None;
                        pending_shared_template = None;
                    }
                    MSG_UTREEXO_STATE => {
                        if reward_mode == RewardModeChoice::Shared {
                            continue;
                        }
                        let state = decode_utreexo_accumulator_state(&frame.payload)?;
                        pre_block_state = Some(state);
                    }
                    MSG_COINBASE_CONTEXT => {
                        if reward_mode == RewardModeChoice::Shared {
                            continue;
                        }
                        let ctx = decode_coinbase_context(&frame.payload)?;
                        coinbase_ctx = Some(ctx);
                    }
                    MSG_NEW_MINING_JOB => {
                        let tmpl = decode_new_template(&frame.payload)?;
                        if reward_mode == RewardModeChoice::Shared {
                            if shared_mode_confirmed {
                                pending_shared_template = None;
                                start_hashing_shared(
                                    tmpl,
                                    share_target,
                                    threads,
                                    Arc::clone(&generation),
                                    Arc::clone(&measured_mhs_x100),
                                    Arc::clone(&sampler_state),
                                    share_tx.clone(),
                                    emitter,
                                );
                            } else {
                                pending_shared_template = Some(tmpl);
                            }
                            continue;
                        }
                        let Some(state) = pre_block_state.clone() else {
                            tracing::warn!("NewMiningJob without UtreexoStateAnnouncement — skipping");
                            continue;
                        };
                        let Some(ctx) = coinbase_ctx.clone() else {
                            tracing::warn!("NewMiningJob without CoinbaseContext — skipping");
                            continue;
                        };
                        start_hashing(
                            tmpl,
                            state,
                            ctx,
                            payout_script.to_vec(),
                            share_target,
                            channel_id,
                            threads,
                            Arc::clone(&cancel),
                            Arc::clone(&generation),
                            Arc::clone(&measured_mhs_x100),
                            Arc::clone(&sampler_state),
                            share_tx.clone(),
                            emitter,
                        );
                    }
                    MSG_WINDOW_STATUS if reward_mode == RewardModeChoice::Shared => {
                        let status = decode_window_status(&frame.payload)?;
                        if status.channel_id != channel_id {
                            continue;
                        }
                        shared_mode_confirmed = true;
                        emitter.emit(
                            "window_status",
                            &serde_json::json!({
                                "channel_id": status.channel_id,
                                "window_bps": status.window_bps,
                                "window_percent": status.window_bps as f64 / 100.0,
                                "window_shares": status.window_shares,
                            }),
                        );
                        if let Some(tmpl) = pending_shared_template.take() {
                            start_hashing_shared(
                                tmpl,
                                share_target,
                                threads,
                                Arc::clone(&generation),
                                Arc::clone(&measured_mhs_x100),
                                Arc::clone(&sampler_state),
                                share_tx.clone(),
                                emitter,
                            );
                        }
                    }
                    MSG_SUBMIT_SHARES_SUCCESS => {
                        let s = decode_submit_shares_success(&frame.payload)?;
                        emitter.emit(
                            "share_accepted",
                            &serde_json::json!({
                                "channel_id": s.channel_id,
                                "last_seq": s.last_sequence_number,
                                "accepted_count": s.new_submits_accepted_count,
                                "shares_sum": s.new_shares_sum,
                            }),
                        );
                    }
                    MSG_SUBMIT_SHARES_ERROR => {
                        let e = decode_submit_shares_error(&frame.payload)?;
                        emitter.emit(
                            "share_rejected",
                            &serde_json::json!({
                                "channel_id": e.channel_id,
                                "sequence_number": e.sequence_number,
                                "error": String::from_utf8_lossy(&e.error_code).to_string(),
                            }),
                        );
                    }
                    MSG_SET_TARGET => {
                        let st = decode_set_target(&frame.payload)?;
                        emitter.emit(
                            "set_target",
                            &serde_json::json!({
                                "channel_id": st.channel_id,
                                "max_target": hex::encode(st.max_target),
                            }),
                        );
                        // Update local target. Bump generation so any
                        // in-flight rayon workers exit; the next
                        // NewMiningJob will respawn with the new target
                        // baked in.
                        share_target = st.max_target;
                        generation.fetch_add(1, Ordering::SeqCst);
                    }
                    other => {
                        tracing::debug!("unhandled frame msg_type=0x{:02x}", other);
                    }
                }
            }
            Some(found) = share_rx.recv() => {
                // Stale result from a superseded generation — drop.
                if found.generation != generation.load(Ordering::SeqCst) {
                    continue;
                }
                seq += 1;
                let job_id = u32::try_from(found.template_id).unwrap_or(0);
                if reward_mode == RewardModeChoice::Shared {
                    let share = SubmitSharesDinero {
                        channel_id,
                        sequence_number: seq,
                        job_id,
                        nonce: found.nonce,
                        timestamp: found.timestamp,
                        version: found.version,
                    };
                    writer
                        .write_frame(MSG_SUBMIT_SHARES_STANDARD, &encode_submit_shares(&share))
                        .await?;
                } else {
                    let ext = SubmitSharesExtendedDinero {
                        channel_id,
                        sequence_number: seq,
                        job_id,
                        nonce: found.nonce,
                        timestamp: found.timestamp,
                        version: found.version,
                        coinbase_outputs: found.coinbase_outputs,
                    };
                    let buf = encode_submit_shares_extended(&ext)?;
                    writer.write_frame(MSG_SUBMIT_SHARES_EXTENDED, &buf).await?;
                }
                emitter.emit(
                    "share_submitted",
                    &serde_json::json!({
                        "sequence_number": seq,
                        "nonce": format!("0x{:08x}", found.nonce),
                        "hash": hex::encode(found.hash),
                        "meets_block_target": found.meets_block_target,
                        "tries": found.tries,
                        "reward_mode": reward_mode.as_str(),
                    }),
                );
                if found.meets_block_target {
                    blocks_found += 1;
                    if args.max_blocks > 0 && blocks_found >= args.max_blocks {
                        break Ok(blocks_found);
                    }
                }
            }
        }
    };

    reader_task.abort();
    // Bump generation so any hashing/telemetry threads still alive from
    // this session see they're stale and exit before the next session
    // (or the same one, on reconnect) spawns fresh workers.
    generation.fetch_add(1, Ordering::SeqCst);
    result
}

async fn expect_setup_success<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut NoiseReader<R>,
) -> Result<()> {
    let f = reader
        .read_frame()
        .await?
        .ok_or_else(|| anyhow::anyhow!("EOF after SetupConnection"))?;
    match f.msg_type {
        MSG_SETUP_CONNECTION_SUCCESS => {
            let _succ = decode_setup_connection_success(&f.payload)?;
            Ok(())
        }
        MSG_SETUP_CONNECTION_ERROR => bail!(
            "SetupConnection.Error: {}",
            String::from_utf8_lossy(&f.payload)
        ),
        other => bail!("unexpected response to SetupConnection: 0x{other:02x}"),
    }
}

async fn expect_channel_open<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut NoiseReader<R>,
) -> Result<(u32, [u8; 32])> {
    let f = reader
        .read_frame()
        .await?
        .ok_or_else(|| anyhow::anyhow!("EOF after OpenStandardMiningChannel"))?;
    match f.msg_type {
        MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS => {
            let succ = decode_open_standard_mining_channel_success(&f.payload)?;
            Ok((succ.channel_id, succ.target))
        }
        MSG_OPEN_STANDARD_MINING_CHANNEL_ERROR => bail!(
            "OpenStandardMiningChannel.Error: {}",
            String::from_utf8_lossy(&f.payload)
        ),
        other => bail!("unexpected response to OpenStandardMiningChannel: 0x{other:02x}"),
    }
}

/// Job snapshot + live nonce position for the FX display sampler.
struct SamplerState {
    job: Mutex<Option<NewTemplateDinero>>,
    nonce_hint: AtomicU64, // low 32 bits nonce; high 32 bits timestamp offset
}

#[derive(Debug)]
struct FoundShare {
    generation: u64,
    template_id: u64,
    timestamp: u64,
    version: u32,
    nonce: u32,
    hash: [u8; 32],
    meets_block_target: bool,
    tries: u64,
    coinbase_outputs: Vec<CoinbaseOutputWire>,
}

/// Build the miner-owned coinbase + post-block Utreexo commitment, then
/// dispatch a rayon parallel nonce sweep. First thread to find a hash
/// under the share target (or the block target, whichever comes first)
/// reports via `share_tx`; the sweep aborts on `cancel` flag.
#[allow(clippy::too_many_arguments)]
fn start_hashing(
    tmpl: NewTemplateDinero,
    pre_block_state: UtreexoAccumulatorState,
    ctx: CoinbaseContext,
    payout_script: Vec<u8>,
    share_target: [u8; 32],
    _channel_id: u32,
    threads: usize,
    _cancel: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    measured_mhs_x100: Arc<AtomicU64>,
    sampler_state: Arc<SamplerState>,
    share_tx: mpsc::UnboundedSender<FoundShare>,
    emitter: &Emitter,
) {
    // Assemble the miner's coinbase outputs (payout + DNRF OP_RETURN).
    let (encoded_filter, _n) = gcs_build(&tmpl.prev_block_hash, &[&payout_script]);
    let filter_hash = gcs_filter_hash(&encoded_filter);
    let dnrf_script = build_dnrf_script(&filter_hash);
    let mut miner_outputs = vec![CoinbaseOutput {
        value_una: ctx.coinbase_value_una,
        script_pubkey: payout_script.clone(),
    }];
    if requires_witness_commitment(ctx.height as u64) {
        if ctx.merkle_path.is_empty() {
            // Coinbase-only block: the witness merkle root is the
            // constant zero hash, so the DNRW commitment is computable
            // without knowing any wtxids.
            miner_outputs.push(CoinbaseOutput {
                value_una: 0,
                script_pubkey: build_dnrw_script_coinbase_only(),
            });
        } else {
            // Mempool txs are in the template; their wtxids aren't in
            // CoinbaseContext, so a correct DNRW can't be built here.
            // Shares still count, but a block-target solution would be
            // rejected by dinerod (missing-witness-commitment).
            tracing::warn!(
                height = ctx.height,
                "non-empty merkle_path: cannot build DNRW commitment; block solutions will not connect"
            );
        }
    }
    if requires_filter_commitment(ctx.height as u64) {
        miner_outputs.push(CoinbaseOutput {
            value_una: 0,
            script_pubkey: dnrf_script,
        });
    }
    let (_coinbase_bytes, coinbase_txid) =
        assemble_stripped_coinbase(&ctx.coinbase_prefix, &miner_outputs, &ctx.coinbase_suffix);
    let mut post_state = pre_block_state.clone();
    for (i, out) in miner_outputs.iter().enumerate() {
        let leaf = leaf_hash_for_height(
            &coinbase_txid,
            i as u32,
            out.value_una,
            &out.script_pubkey,
            ctx.height,
            true,
            UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET,
        );
        if let Err(err) = post_state.add_leaf(leaf) {
            tracing::error!("post-state add_leaf failed: {err}");
            return;
        }
    }
    let our_utreexo_root = match utreexo_commitment(&post_state) {
        Ok(v) => v,
        Err(err) => {
            tracing::error!("utreexo_commitment failed: {err}");
            return;
        }
    };
    let merkle_root = compute_root(coinbase_txid, &ctx.merkle_path);
    let our_template = NewTemplateDinero {
        template_id: tmpl.template_id,
        future_template: tmpl.future_template,
        version: tmpl.version,
        prev_block_hash: tmpl.prev_block_hash,
        merkle_root,
        utreexo_root: our_utreexo_root,
        timestamp: tmpl.timestamp,
        difficulty: tmpl.difficulty,
        coinbase_outputs_commitment: [0u8; 32],
    };

    let coinbase_outputs_wire: Vec<CoinbaseOutputWire> = miner_outputs
        .iter()
        .map(|o| CoinbaseOutputWire {
            value_una: o.value_una,
            script_pubkey: o.script_pubkey.clone(),
        })
        .collect();

    emitter.emit(
        "new_job",
        &serde_json::json!({
            "template_id": tmpl.template_id,
            "height": ctx.height,
            "coinbase_value_una": ctx.coinbase_value_una,
            "utreexo_root": hex::encode(our_utreexo_root),
            "merkle_root": hex::encode(merkle_root),
            "difficulty_nbits": format!("0x{:08x}", tmpl.difficulty),
            "block_target": hex::encode(nbits_to_target(tmpl.difficulty)),
            "share_target": hex::encode(share_target),
            "reward_mode": "solo",
        }),
    );

    start_hashing_template(
        our_template,
        coinbase_outputs_wire,
        share_target,
        threads,
        generation,
        measured_mhs_x100,
        sampler_state,
        share_tx,
        emitter,
    );
}

#[allow(clippy::too_many_arguments)]
fn start_hashing_shared(
    tmpl: NewTemplateDinero,
    share_target: [u8; 32],
    threads: usize,
    generation: Arc<AtomicU64>,
    measured_mhs_x100: Arc<AtomicU64>,
    sampler_state: Arc<SamplerState>,
    share_tx: mpsc::UnboundedSender<FoundShare>,
    emitter: &Emitter,
) {
    emitter.emit(
        "new_job",
        &serde_json::json!({
            "template_id": tmpl.template_id,
            "utreexo_root": hex::encode(tmpl.utreexo_root),
            "merkle_root": hex::encode(tmpl.merkle_root),
            "difficulty_nbits": format!("0x{:08x}", tmpl.difficulty),
            "block_target": hex::encode(nbits_to_target(tmpl.difficulty)),
            "share_target": hex::encode(share_target),
            "reward_mode": "shared",
        }),
    );

    start_hashing_template(
        tmpl,
        Vec::new(),
        share_target,
        threads,
        generation,
        measured_mhs_x100,
        sampler_state,
        share_tx,
        emitter,
    );
}

#[allow(clippy::too_many_arguments)]
fn start_hashing_template(
    tmpl: NewTemplateDinero,
    coinbase_outputs_wire: Vec<CoinbaseOutputWire>,
    share_target: [u8; 32],
    threads: usize,
    generation: Arc<AtomicU64>,
    measured_mhs_x100: Arc<AtomicU64>,
    sampler_state: Arc<SamplerState>,
    share_tx: mpsc::UnboundedSender<FoundShare>,
    emitter: &Emitter,
) {
    let gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let block_target = nbits_to_target(tmpl.difficulty);

    // Snapshot the job for the FX display's sampler. In non-FX modes this
    // is a cheap, harmless write — nothing ever reads it back.
    *sampler_state.job.lock().unwrap() = Some(tmpl.clone());

    let tmpl_timestamp = tmpl.timestamp;
    let tmpl_version = tmpl.version;
    let tmpl_id = tmpl.template_id;

    // Shared hash counter + a telemetry thread that reports the rolling
    // hashrate via the structured emitter every 2 seconds. Sibling of the
    // GPU miner's hashrate event so Qt can treat both backends uniformly.
    // Both this thread and the rayon workers below exit when they observe
    // a generation bump — that's the single cancellation signal, replacing
    // the bool flag (which had a clear-before-observe race and orphaned
    // telemetry threads on same-tip refresh, each spamming `mhs:0`).
    let hashes_done = Arc::new(AtomicU64::new(0));
    {
        let hashes = Arc::clone(&hashes_done);
        let global_generation = Arc::clone(&generation);
        let measured = Arc::clone(&measured_mhs_x100);
        let gen_at_spawn = gen;
        let emitter = emitter.clone();
        std::thread::spawn(move || {
            let mut last = 0u64;
            let mut last_instant = Instant::now();
            loop {
                std::thread::sleep(Duration::from_secs(2));
                if global_generation.load(Ordering::Relaxed) > gen_at_spawn {
                    return;
                }
                let now = hashes.load(Ordering::Relaxed);
                let dt = last_instant.elapsed().as_secs_f64();
                let delta = now.saturating_sub(last);
                let mhs = (delta as f64 / dt) / 1e6;
                // Publish to the process-wide rolling rate so the next
                // reconnect's `nominal_hash_rate_bits` is grounded in
                // reality, not the static `threads * 3 MH/s` ballpark.
                measured.store((mhs * 100.0).round() as u64, Ordering::Relaxed);
                emitter.emit(
                    "hashrate",
                    &serde_json::json!({
                        "mhs": (mhs * 100.0).round() / 100.0,
                        "hashes_since_last": delta,
                        "interval_s": (dt * 100.0).round() / 100.0,
                        "backend": "cpu",
                    }),
                );
                last = now;
                last_instant = Instant::now();
            }
        });
    }

    let global_generation = Arc::clone(&generation);
    let sampler_for_sweep = Arc::clone(&sampler_state);
    std::thread::spawn(move || {
        let per_thread = (u32::MAX as u64 + 1) / (threads.max(1) as u64);
        let mut ranges: Vec<(u32, u32)> = Vec::with_capacity(threads);
        let mut cursor: u64 = 0;
        for i in 0..threads {
            let start = cursor as u32;
            let end = if i == threads - 1 {
                u32::MAX
            } else {
                (cursor + per_thread - 1) as u32
            };
            ranges.push((start, end));
            cursor += per_thread;
        }

        // Per-rayon-worker:
        //   outer loop  : bump local timestamp when the assigned range exhausts
        //   inner loop  : sweep [start, end] at the current timestamp
        // Each share carries its own timestamp on the wire, so different
        // workers running at different timestamps doesn't desync the pool —
        // it just means we explore a wider (timestamp, nonce) plane in
        // parallel, identical in spirit to the GPU miner's wrap fix.
        ranges.par_iter().for_each(|(start, end)| {
            let mut local_hashes: u64 = 0;
            let mut current_timestamp: u64 = tmpl_timestamp;
            'outer: loop {
                if global_generation.load(Ordering::Relaxed) > gen {
                    hashes_done.fetch_add(local_hashes, Ordering::Relaxed);
                    return;
                }
                let mut nonce = *start;
                let mut tries: u64 = 0;
                loop {
                    if global_generation.load(Ordering::Relaxed) > gen {
                        hashes_done.fetch_add(local_hashes, Ordering::Relaxed);
                        return;
                    }
                    if tries & 0xFFFFF == 0 && tries > 0 {
                        hashes_done.fetch_add(local_hashes, Ordering::Relaxed);
                        local_hashes = 0;
                    }

                    let share = SubmitSharesDinero {
                        channel_id: 0,
                        sequence_number: 0,
                        job_id: 0,
                        nonce,
                        timestamp: current_timestamp,
                        version: tmpl_version,
                    };
                    let hash = HeaderAssembly::hash(&tmpl, &share);
                    local_hashes += 1;
                    if hash < share_target {
                        let meets_block = hash < block_target;
                        let _ = share_tx.send(FoundShare {
                            generation: gen,
                            template_id: tmpl_id,
                            timestamp: current_timestamp,
                            version: tmpl_version,
                            nonce,
                            hash,
                            meets_block_target: meets_block,
                            tries,
                            coinbase_outputs: coinbase_outputs_wire.clone(),
                        });
                    }
                    tries += 1;
                    if tries & 0x3FFFF == 0 {
                        sampler_for_sweep.nonce_hint.store(
                            ((current_timestamp - tmpl_timestamp) << 32) | nonce as u64,
                            Ordering::Relaxed,
                        );
                    }
                    if nonce == *end {
                        hashes_done.fetch_add(local_hashes, Ordering::Relaxed);
                        local_hashes = 0;
                        break;
                    }
                    nonce = nonce.wrapping_add(1);
                }

                // Bump timestamp and re-sweep this worker's range. Pool
                // accepts shares with miner-supplied ntime within the
                // daemon's future-block-time tolerance; capping at +3600 s
                // matches the GPU miner.
                current_timestamp = current_timestamp.wrapping_add(1);
                if current_timestamp.saturating_sub(tmpl_timestamp) > 3600 {
                    // Overshot the future-time tolerance with no new job —
                    // park until generation flips.
                    while global_generation.load(Ordering::Relaxed) == gen {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    return;
                }
                continue 'outer;
            }
        });
    });
}

// ───── Structured event emitter ─────
//
// `json` OR a non-TTY stdout routes through `OutputMode::Json` /
// `OutputMode::Plain` — byte-identical to the pre-Task-6 emitter, since
// GUI wrappers (dinero-qt) and log-redirected runs depend on that wire
// format never drifting. A TTY with `--json` absent gets `OutputMode::
// Human`, which translates the exact same `emit(event, data)` calls into
// `Display`-rendered lines instead of JSON/plain text.

#[derive(Clone)]
struct Emitter {
    mode: OutputMode,
}

#[derive(Clone)]
enum OutputMode {
    Json,
    Plain,
    Human(Arc<Mutex<HumanState>>),
    Fx(dinero_miner_ux::fx::FxScreen),
}

impl Emitter {
    fn new(json: bool, human: bool) -> Self {
        let mode = if json {
            OutputMode::Json
        } else if human {
            OutputMode::Human(Arc::new(Mutex::new(HumanState::new())))
        } else {
            OutputMode::Plain
        };
        Self { mode }
    }

    /// FX mode's `FxScreen` is constructed (and its ticker spawned) by the
    /// caller in `async_main` before the reconnect loop — this just wraps
    /// it as the emitter's routing target.
    fn new_fx(fx: dinero_miner_ux::fx::FxScreen) -> Self {
        Self {
            mode: OutputMode::Fx(fx),
        }
    }

    fn emit_startup(&self, pool: &str, server_pubkey_pinned: bool, threads: usize, user_agent: &str) {
        self.emit(
            "startup",
            &serde_json::json!({
                "pool": pool,
                "server_pubkey_pinned": server_pubkey_pinned,
                "threads": threads,
                "user_agent": user_agent,
                "version": env!("CARGO_PKG_VERSION"),
            }),
        );
    }

    fn emit(&self, event: &str, data: &serde_json::Value) {
        match &self.mode {
            OutputMode::Json => {
                let mut map = data_as_object(data);
                map.insert(
                    "event".to_string(),
                    serde_json::Value::String(event.to_string()),
                );
                let line = serde_json::Value::Object(map);
                println!("{}", line);
            }
            OutputMode::Plain => {
                println!("[{event}] {data}");
            }
            OutputMode::Human(state) => emit_human(state, event, data),
            OutputMode::Fx(fx) => match event {
                "hashrate" => { if let Some(mhs) = data.get("mhs").and_then(|v| v.as_f64()) { fx.on_hashrate(mhs); } }
                "share_accepted" => fx.on_share_ok(data.get("accepted_count").and_then(|v| v.as_u64()).unwrap_or(1)),
                "share_rejected" => fx.on_share_rejected(),
                "window_status" => { if let Some(bps) = data.get("window_bps").and_then(|v| v.as_u64()) { fx.on_window(bps); } }
                "new_job" => {
                    // Solo templates carry the exact coinbase value for the DIN total.
                    if let Some(una) = data.get("coinbase_value_una").and_then(|v| v.as_u64()) { fx.on_solo_job_value(una); }
                    // deliberately NOT surfaced as a lifecycle line — job churn would
                    // spam the permanent history; the feed itself shows the work.
                }
                "set_new_prev_hash" => { /* same: silent in FX mode */ }
                "share_submitted" => {
                    if data.get("meets_block_target").and_then(|v| v.as_bool()).unwrap_or(false) {
                        fx.on_block(data.get("hash").and_then(|v| v.as_str()).unwrap_or(""), &now_hms());
                    }
                }
                "connected" => fx.lifecycle_state(&lifecycle_line(event, data), Some("ONLINE"), None, None, false),
                "channel_open" => fx.lifecycle_state(
                    &lifecycle_line(event, data), Some("ONLINE"),
                    data.get("channel_id").and_then(|v| v.as_u64()), None, false),
                "set_target" => fx.lifecycle_state(
                    &lifecycle_line(event, data), None, None,
                    data.get("max_target").and_then(|v| v.as_str()).map(str::to_string), false),
                "session_end" => fx.lifecycle_state(&lifecycle_line(event, data), Some("OFFLINE"), None, None, false),
                "reconnect_wait" => fx.lifecycle_state(&lifecycle_line(event, data), Some("RECONNECTING"), None, None, true),
                _ => fx.lifecycle(&lifecycle_line(event, data)),
            },
        }
    }

    /// FX mode's non-interactive exits (max-blocks reached, no-reconnect
    /// clean close, no-reconnect fatal error) call this before returning
    /// from `async_main`'s reconnect loop so the painted region is cleared
    /// and a summary line lands instead of gluing to the shell prompt —
    /// the same cleanup Ctrl-C already gets via `print_summary`. No-op in
    /// Json/Plain/Human mode, matching prior behavior exactly.
    fn print_fx_summary(&self) {
        if let OutputMode::Fx(fx) = &self.mode {
            fx.print_summary();
        }
    }

    /// In human mode, listens for Ctrl-C and prints a final session summary
    /// before exiting — the human-display equivalent of the JSON path's
    /// `session_end` line. No-op (process just dies on SIGINT as before) in
    /// JSON/plain mode, matching prior behavior exactly.
    fn spawn_ctrlc_summary_handler(&self) {
        match &self.mode {
            OutputMode::Human(state) => {
                let state = Arc::clone(state);
                tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    if let Ok(st) = state.lock() {
                        let elapsed = st.stats.started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                        println!();
                        println!("{}", Display::session_summary(&st.stats, elapsed));
                        let _ = std::io::stdout().flush();
                    }
                    std::process::exit(0);
                });
            }
            OutputMode::Fx(fx) => {
                let fx = fx.clone();
                tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    fx.print_summary();
                    std::process::exit(0);
                });
            }
            OutputMode::Json | OutputMode::Plain => {}
        }
    }
}

/// Flatten `data` into the top-level JSON line if it's an object; else
/// nest under a `data` key.
fn data_as_object(data: &serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match data {
        serde_json::Value::Object(map) => map.clone(),
        other => {
            let mut map = serde_json::Map::new();
            map.insert("data".to_string(), other.clone());
            map
        }
    }
}

// ───── Human display ─────

/// Live state behind `OutputMode::Human`: rolling session stats plus enough
/// bookkeeping to repaint the self-overwriting status line cleanly. Shared
/// (`Arc<Mutex<_>>`) because the telemetry thread in
/// `start_hashing_template` clones the `Emitter` into its own OS thread.
struct HumanState {
    stats: SessionStats,
    last_line_len: usize,
    status_live: bool,
}

impl HumanState {
    fn new() -> Self {
        let stats = SessionStats {
            started: Some(Instant::now()),
            ..Default::default()
        };
        Self {
            stats,
            last_line_len: 0,
            status_live: false,
        }
    }

    /// Repaints the status line in place via `\r`, padding with trailing
    /// spaces to the previous line's length so a shorter line doesn't leave
    /// stale glyphs from the longer one behind.
    fn repaint_status(&mut self, out: &mut impl Write) {
        let line = Display::status_line(&self.stats);
        let pad = self.last_line_len.saturating_sub(line.chars().count());
        let _ = write!(out, "\r{line}{}", " ".repeat(pad));
        let _ = out.flush();
        self.last_line_len = line.chars().count();
        self.status_live = true;
    }

    /// Clears a live status line before printing something that must not
    /// share its row (a lifecycle line or a block banner).
    fn clear_status(&mut self, out: &mut impl Write) {
        if self.status_live {
            let _ = write!(out, "\r{}\r", " ".repeat(self.last_line_len));
            let _ = out.flush();
            self.status_live = false;
        }
    }
}

fn emit_human(state: &Arc<Mutex<HumanState>>, event: &str, data: &serde_json::Value) {
    let Ok(mut st) = state.lock() else { return };
    let mut stdout = std::io::stdout();
    match event {
        "hashrate" => {
            if let Some(mhs) = data.get("mhs").and_then(|v| v.as_f64()) {
                st.stats.hashrate_hs = mhs * 1e6;
            }
            st.repaint_status(&mut stdout);
        }
        "share_accepted" => {
            let n = data
                .get("accepted_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(1);
            st.stats.ok += n;
            st.repaint_status(&mut stdout);
        }
        "share_rejected" => {
            st.stats.rej += 1;
            st.repaint_status(&mut stdout);
        }
        "share_submitted" => {
            let meets_block = data
                .get("meets_block_target")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if meets_block {
                st.stats.blocks += 1;
                st.clear_status(&mut stdout);
                let hash = data.get("hash").and_then(|v| v.as_str()).unwrap_or("");
                let nonce = data.get("nonce").and_then(|v| v.as_str()).unwrap_or("");
                let tries = data.get("tries").and_then(|v| v.as_u64()).unwrap_or(0);
                let mode = data
                    .get("reward_mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let banner =
                    Display::block_banner(st.stats.blocks, hash, nonce, tries, mode, &now_hms());
                print!("{banner}");
                let _ = stdout.flush();
            }
        }
        _ => {
            st.clear_status(&mut stdout);
            println!("{}", lifecycle_line(event, data));
        }
    }
}

/// Generic one-line rendering for events that aren't the status line or
/// the block banner (connection lifecycle, job/window/share-error
/// notices, startup, session end, ...).
fn lifecycle_line(event: &str, data: &serde_json::Value) -> String {
    let parts: Vec<String> = match data {
        serde_json::Value::Object(map) => map
            .iter()
            .map(|(k, v)| format!("{k}={}", compact_value(v)))
            .collect(),
        _ => Vec::new(),
    };
    if parts.is_empty() {
        format!("»  {event}")
    } else {
        format!("»  {event}  {}", parts.join("  "))
    }
}

fn compact_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `HH:MM:SS` wall-clock time for the block banner. UTC (no timezone
/// database is linked in), computed by hand to avoid pulling in a chrono/
/// time dependency for a single cosmetic timestamp.
fn now_hms() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let day_secs = secs % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60
    )
}

#[cfg(test)]
mod reward_mode_tests {
    use super::*;

    #[test]
    fn cli_reward_mode_unspecified_is_none_but_accepts_shared() {
        let base = [
            "miner",
            "--pool",
            "127.0.0.1:4444",
            "--payout-script-hex",
            "51200000000000000000000000000000000000000000000000000000000000000000",
        ];
        assert_eq!(Args::try_parse_from(base).unwrap().reward_mode, None);

        let shared = base.into_iter().chain(["--reward-mode", "shared"]);
        assert_eq!(
            Args::try_parse_from(shared).unwrap().reward_mode,
            Some(RewardModeChoice::Shared)
        );
    }

    #[test]
    fn unspecified_reward_mode_resolves_to_shared() {
        // Behavior change (this task): bare invocation used to default to
        // solo at the clap level; now clap yields None and the config
        // resolver's default is shared.
        assert_eq!(resolve_reward_mode("garbage-unknown-mode"), RewardModeChoice::Shared);
        assert_eq!(
            dinero_miner_ux::config::resolve(
                &dinero_miner_ux::config::FileConfig::default(),
                &dinero_miner_ux::config::FileConfig::default(),
                4
            )
            .reward_mode,
            "shared"
        );
    }

    #[test]
    fn shared_requires_taproot_but_solo_keeps_existing_scripts() {
        let p2tr = [0x51u8; 34];
        let mut valid_p2tr = p2tr;
        valid_p2tr[1] = 0x20;
        assert!(validate_reward_payout(RewardModeChoice::Shared, &valid_p2tr).is_ok());
        assert!(validate_reward_payout(RewardModeChoice::Shared, &[0x52, 0x20]).is_err());
        assert!(validate_reward_payout(RewardModeChoice::Solo, &[0x52, 0x20]).is_ok());
    }
}

#[cfg(test)]
mod args_tests {
    use super::*;

    #[test]
    fn address_and_script_are_mutually_exclusive() {
        assert!(Args::try_parse_from([
            "m",
            "--address",
            "din1p...x",
            "--payout-script-hex",
            "51ab"
        ])
        .is_err());
    }

    #[test]
    fn bare_invocation_parses() {
        let a = Args::try_parse_from(["m"]).unwrap();
        assert!(a.pool.is_none() && a.address.is_none() && a.reward_mode.is_none());
    }

    #[test]
    fn pool_accepts_hostname_endpoints() {
        let a = Args::try_parse_from(["m", "--pool", "pool.dinerolabs.org:4444"]).unwrap();
        assert_eq!(a.pool.as_deref(), Some("pool.dinerolabs.org:4444"));
        assert!(validate_pool_endpoint("pool.dinerolabs.org:4444").is_ok());
        assert!(validate_pool_endpoint("173.249.200.59:4444").is_ok());
        assert!(validate_pool_endpoint("no-port-here").is_err());
        assert!(validate_pool_endpoint(":4444").is_err());
        assert!(validate_pool_endpoint("host:notaport").is_err());
    }

    #[test]
    fn invalid_address_fatal_from_flag_recoverable_from_config() {
        const GOOD: &str = "din1pafzgzwwfeqkfh7u4kkpe8qy97gey3zcvymx5eumxzx45m08q6tgqedz700";
        const BAD: &str = "din1p-not-a-real-address";
        assert!(matches!(
            classify_resolved_address(None, GOOD),
            AddressOutcome::Ready(_)
        ));
        assert!(matches!(
            classify_resolved_address(Some(BAD), BAD),
            AddressOutcome::Fatal(_)
        ));
        assert!(matches!(
            classify_resolved_address(None, BAD),
            AddressOutcome::Recoverable(_)
        ));
    }

    #[test]
    fn plain_flag_parses() {
        assert!(Args::try_parse_from(["m", "--plain"]).unwrap().plain);
        assert!(!Args::try_parse_from(["m"]).unwrap().plain);
    }

    #[test]
    fn mode_selection_rules() {
        // pure helper: (json, tty, plain, term_ok) -> Mode
        use ModeChoice::*;
        assert_eq!(choose_mode(true, true, false, true), Json);
        assert_eq!(choose_mode(false, false, false, true), PlainMachine);
        assert_eq!(choose_mode(false, true, true, true), HumanV1);
        assert_eq!(choose_mode(false, true, false, false), HumanV1);
        assert_eq!(choose_mode(false, true, false, true), Fx);
    }
}
