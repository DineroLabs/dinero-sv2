//! Dinero Stratum V2 GPU miner (Metal / OpenCL / CUDA backends).
//!
//! Speaks Noise NX + SV2 + Job Declaration to a pool like the LA reference
//! pool. Unlike `dinero-sv2-miner` (CPU), this binary dispatches the nonce
//! sweep to the Apple GPU via a Metal compute kernel.
//!
//! The SV2 session code mirrors `dinero-sv2-miner/src/main.rs` — duplicated
//! rather than shared so the CPU miner stays untouched. The only mining-
//! relevant difference is `start_hashing_gpu` replacing the CPU rayon sweep.
//!
//! The GPU backend is selected at runtime via `--backend auto|metal|opencl|cuda`
//! over the shared `GpuBackend` abstraction (see `backend.rs`).

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
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::sleep;

#[cfg(target_os = "macos")]
mod metal_backend;

#[cfg(not(target_os = "macos"))]
mod opencl_backend;

#[cfg(feature = "cuda")]
mod cuda_backend;

/// Shared GPU backend abstraction (DispatchOutcome, GpuBackend, pack_target_be).
mod backend;

use backend::GpuBackend;

/// Which GPU backend to use. `auto` picks the best available for the host
/// (Metal on macOS; otherwise CUDA when present, else OpenCL). Explicit
/// values force a backend and hard-error if it is unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum BackendChoice {
    Auto,
    Metal,
    Opencl,
    Cuda,
}

/// Pure backend decision (no GPU init), so it is unit-testable. Returns the
/// name of the backend to construct, or an actionable error.
fn choose_backend(
    choice: BackendChoice,
    is_macos: bool,
    cuda_available: bool,
    opencl_available: bool,
) -> std::result::Result<&'static str, String> {
    match choice {
        BackendChoice::Metal if is_macos => Ok("metal"),
        BackendChoice::Metal => Err("metal backend is macOS-only".into()),
        BackendChoice::Cuda if cuda_available => Ok("cuda"),
        BackendChoice::Cuda => Err("CUDA backend requested but unavailable — install the \
             NVIDIA driver + CUDA runtime, or use --backend opencl / the CPU miner \
             dinero-sv2-miner"
            .into()),
        BackendChoice::Opencl if opencl_available => Ok("opencl"),
        BackendChoice::Opencl => {
            Err("OpenCL requested but no OpenCL GPU platform was found".into())
        }
        BackendChoice::Auto if is_macos => Ok("metal"),
        BackendChoice::Auto if cuda_available => Ok("cuda"),
        BackendChoice::Auto if opencl_available => Ok("opencl"),
        BackendChoice::Auto => Err("no usable GPU backend found (no CUDA/OpenCL) — use the \
             CPU miner dinero-sv2-miner"
            .into()),
    }
}

/// Construct the selected backend at runtime. Availability is probed by
/// attempting `init()`; under `auto` an unavailable CUDA/OpenCL is skipped,
/// while an explicit `--backend` that is unavailable hard-errors.
#[cfg(target_os = "macos")]
fn build_backend(choice: BackendChoice) -> Result<Arc<dyn GpuBackend>> {
    // macOS exposes only Metal (OpenCL/CUDA modules are compiled out).
    match choose_backend(choice, true, false, false).map_err(|e| anyhow::anyhow!(e))? {
        "metal" => Ok(Arc::new(
            metal_backend::MetalMiner::init().context("metal init")?,
        )),
        other => bail!("backend {other} is not available on macOS"),
    }
}

#[cfg(not(target_os = "macos"))]
fn build_backend(choice: BackendChoice) -> Result<Arc<dyn GpuBackend>> {
    #[cfg(feature = "cuda")]
    let cuda = cuda_backend::CudaMiner::init().ok();
    #[cfg(feature = "cuda")]
    let cuda_available = cuda.is_some();
    #[cfg(not(feature = "cuda"))]
    let cuda_available = false;

    let opencl = opencl_backend::OpenClMiner::init().ok();
    let opencl_available = opencl.is_some();

    match choose_backend(choice, false, cuda_available, opencl_available)
        .map_err(|e| anyhow::anyhow!(e))?
    {
        #[cfg(feature = "cuda")]
        "cuda" => Ok(Arc::new(cuda.expect("cuda probed available"))),
        "opencl" => Ok(Arc::new(opencl.expect("opencl probed available"))),
        other => bail!("backend {other} is not available on this platform"),
    }
}

/// GPU backends compiled into THIS binary, from build cfg. This is the
/// per-platform truth the dinero-qt selector reads via `--print-backends`.
fn compiled_backends() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        v.push("metal");
    }
    #[cfg(not(target_os = "macos"))]
    {
        v.push("opencl");
        #[cfg(feature = "cuda")]
        {
            v.push("cuda");
        }
    }
    v
}

/// Capability report: which backends are compiled in, which `init()` right now,
/// and the reason for any that don't. Serialized by `--print-backends` and read
/// by the dinero-qt selector to enable/grey-out backend choices.
fn backend_report() -> serde_json::Value {
    let compiled = compiled_backends();
    let mut available: Vec<&'static str> = Vec::new();
    let mut unavailable = serde_json::Map::new();
    {
        let mut probe = |name: &'static str, res: Result<()>| match res {
            Ok(()) => available.push(name),
            Err(e) => {
                unavailable.insert(name.to_string(), serde_json::Value::from(e.to_string()));
            }
        };
        #[cfg(target_os = "macos")]
        probe("metal", metal_backend::MetalMiner::init().map(|_| ()));
        #[cfg(not(target_os = "macos"))]
        {
            probe("opencl", opencl_backend::OpenClMiner::init().map(|_| ()));
            #[cfg(feature = "cuda")]
            probe("cuda", cuda_backend::CudaMiner::init().map(|_| ()));
        }
    }
    serde_json::json!({
        "compiled": compiled,
        "available": available,
        "unavailable": serde_json::Value::Object(unavailable),
    })
}

#[cfg(test)]
mod backend_select_tests {
    use super::{choose_backend, BackendChoice};

    #[test]
    fn auto_prefers_cuda_then_opencl_off_macos() {
        assert_eq!(
            choose_backend(BackendChoice::Auto, false, true, true),
            Ok("cuda")
        );
        assert_eq!(
            choose_backend(BackendChoice::Auto, false, false, true),
            Ok("opencl")
        );
    }

    #[test]
    fn auto_uses_metal_on_macos() {
        assert_eq!(
            choose_backend(BackendChoice::Auto, true, false, false),
            Ok("metal")
        );
    }

    #[test]
    fn auto_errors_when_nothing_available() {
        assert!(choose_backend(BackendChoice::Auto, false, false, false).is_err());
    }

    #[test]
    fn explicit_cuda_without_cuda_errors() {
        assert!(choose_backend(BackendChoice::Cuda, false, false, true).is_err());
    }

    #[test]
    fn explicit_metal_off_macos_errors() {
        assert!(choose_backend(BackendChoice::Metal, false, false, true).is_err());
    }

    #[test]
    fn explicit_opencl_ok_when_available() {
        assert_eq!(
            choose_backend(BackendChoice::Opencl, false, false, true),
            Ok("opencl")
        );
    }
}

#[cfg(test)]
mod print_backends_tests {
    use super::{backend_report, compiled_backends};

    #[test]
    fn report_has_three_keys_and_compiled_nonempty() {
        let r = backend_report();
        assert!(r.get("compiled").and_then(|v| v.as_array()).is_some());
        assert!(r.get("available").and_then(|v| v.as_array()).is_some());
        assert!(r.get("unavailable").and_then(|v| v.as_object()).is_some());
        assert!(!r["compiled"].as_array().unwrap().is_empty());
        // Every compiled backend is exactly one of available / unavailable.
        for b in r["compiled"].as_array().unwrap() {
            let name = b.as_str().unwrap();
            let avail = r["available"].as_array().unwrap().iter().any(|x| x == b);
            let unavail = r["unavailable"].as_object().unwrap().contains_key(name);
            assert!(
                avail ^ unavail,
                "{name} must be exactly one of available/unavailable"
            );
        }
    }

    #[test]
    fn compiled_reflects_platform_cfg() {
        let c = compiled_backends();
        #[cfg(target_os = "macos")]
        {
            assert_eq!(c, vec!["metal"]);
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(c.contains(&"opencl"), "opencl always compiled off-macOS");
            assert_eq!(c.contains(&"cuda"), cfg!(feature = "cuda"));
        }
        assert!(!c.is_empty());
    }
}

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
#[command(version, about = "Dinero SV2 GPU pool miner (Metal/OpenCL)")]
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
    /// Legacy alternative to --address; bypasses address validation.
    #[arg(long)]
    payout_script_hex: Option<String>,

    /// Reward ownership: solo uses a miner-owned coinbase; shared submits
    /// standard shares to the pool's PPLNS window. Unspecified resolves to
    /// shared; pass this flag (or set it in the saved config) to mine solo.
    #[arg(long, value_enum)]
    reward_mode: Option<RewardModeChoice>,

    /// Skip writing the resolved address/pool/pubkey/mode to the config
    /// file. Useful for tests and scripted one-off runs.
    #[arg(long)]
    no_save: bool,

    /// GPU backend: auto (default), metal, opencl, or cuda.
    #[arg(long, value_enum, default_value = "auto")]
    backend: BackendChoice,

    #[arg(long, default_value = "dinero-sv2-gpu-miner")]
    user_agent: String,

    /// Nonces per Metal dispatch. Larger = less overhead but longer time
    /// to respond to new jobs. Default 1M is ~3-15 ms on Apple Silicon
    /// depending on die.
    #[arg(long, default_value_t = 1u32 << 20)]
    batch_size: u32,

    #[arg(long)]
    json: bool,

    #[arg(long, default_value_t = 5)]
    reconnect_secs: u64,

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
    // Early-exit capability probe: print the backend report and exit before
    // clap requires --pool/--payout-script-hex (a raw-argv pre-scan keeps the
    // mining args strongly typed). The dinero-qt selector calls this to learn
    // which backends are compiled + available, and greys out the rest.
    if std::env::args().skip(1).any(|a| a == "--print-backends") {
        println!(
            "{}",
            serde_json::to_string(&backend_report()).expect("serialize backend report")
        );
        return Ok(());
    }
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
    // The GPU miner has no --threads flag; the resolver's cores argument
    // only feeds its (unused here) threads default, so pass 1.
    let effective = dinero_miner_ux::config::resolve(&flags, &file, 1);
    let reward_mode = resolve_reward_mode(&effective.reward_mode);

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
    // config path) when it came from the saved file, not from `--address`.
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
            },
        );

        // Build the real-hash sampler and spawn the ticker right after
        // constructing the FxScreen — Task 5's review found `on_block`'s
        // erase math assumes the region has been painted at least once
        // before a block event, which the ticker's first tick guarantees.
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

        fx.print_banner();
        Emitter::new_fx(fx)
    } else {
        Emitter::new(args.json, human, reward_mode.as_str())
    };
    emitter.spawn_ctrlc_summary_handler();
    emitter.emit_startup(&args, &pool, pinned.is_some());

    let gpu = build_backend(args.backend).context("gpu init")?;
    emitter.emit(
        "gpu_ready",
        &serde_json::json!({
            "device": gpu.device_name(),
            "max_threads_per_group": gpu.max_threads_per_group(),
            "batch_size": args.batch_size,
            "backend": gpu.name(),
        }),
    );

    // Process-wide generation counter (lives across reconnects) so
    // hashing threads from a previous session observe a bump and
    // exit; otherwise their per-session generation Arc would never
    // increment and the GPU dispatch thread would run forever.
    let generation = Arc::new(AtomicU64::new(0));

    // Rolling measured MH/s × 100 (centi-MH/s, integer atomic). Updated
    // by the GPU hash thread on each telemetry tick, read at each
    // reconnect so OpenStandardMiningChannel.nominal_hash_rate_bits
    // reports the real rate instead of a hardcoded 100 MH/s ballpark.
    let measured_mhs_x100 = Arc::new(AtomicU64::new(0));

    let mut blocks_found: u64 = 0;
    loop {
        match run_session(
            &args,
            &pool,
            reward_mode,
            pinned.as_ref(),
            &payout_script,
            &gpu,
            Arc::clone(&generation),
            Arc::clone(&measured_mhs_x100),
            Arc::clone(&sampler_state),
            &emitter,
        )
        .await
        {
            Ok(found) => {
                blocks_found += found;
                if args.max_blocks > 0 && blocks_found >= args.max_blocks {
                    emitter.emit(
                        "session_end",
                        &serde_json::json!({"reason": "max-blocks-reached"}),
                    );
                    return Ok(());
                }
                emitter.emit("session_end", &serde_json::json!({"reason": "clean-close"}));
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
                    return Err(err);
                }
            }
        }
        if args.reconnect_secs == 0 {
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
        // No --threads flag on the GPU miner; never touch the saved value.
        threads: None,
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
        eprintln!("⛏  Dinero SV2 GPU Miner");
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
    let script_hex = dinero_miner_ux::address::payout_script_hex(addr)
        .map_err(|e| anyhow::anyhow!(e.message()))?;
    hex::decode(script_hex).context("payout_script_hex produced non-hex output")
}

/// Persists the resolved address (always, when present) plus any
/// explicitly-flagged pool/pubkey/mode, merged onto whatever was already
/// on disk so unrelated saved settings survive a run that didn't touch
/// them. Best-effort: a write failure is a warning, not a fatal error.
fn save_config(
    path: &std::path::Path,
    file: &FileConfig,
    args: &Args,
    resolved_address: Option<String>,
) {
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

#[allow(clippy::too_many_arguments)]
async fn run_session(
    args: &Args,
    pool: &str,
    reward_mode: RewardModeChoice,
    pinned: Option<&[u8; 32]>,
    payout_script: &[u8],
    gpu: &Arc<dyn GpuBackend>,
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
            "backend": gpu.name(),
        }),
    );

    let (mut reader, mut writer) = session.split();

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

    // Use the rolling measured rate from the previous session for the
    // SV2 channel-open declaration; first connect (atomic still 0)
    // falls back to the 100 MH/s ballpark so the pool has a non-zero
    // value to size targets from.
    let measured = measured_mhs_x100.load(Ordering::Relaxed);
    let nominal_hps = if measured > 0 {
        (measured as f32 / 100.0) * 1_000_000.0
    } else {
        100_000_000.0
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

    let mut pre_block_state: Option<UtreexoAccumulatorState> = None;
    let mut coinbase_ctx: Option<CoinbaseContext> = None;
    let mut pending_shared_template: Option<NewTemplateDinero> = None;
    let mut shared_mode_confirmed = false;
    let mut blocks_found: u64 = 0;
    let mut seq: u32 = 0;

    let (share_tx, mut share_rx) = mpsc::unbounded_channel::<FoundShare>();

    // Coalesce share-accept telemetry. With the GPU running at ~500
    // shares/sec, emitting a JSON event per pool ack drowns the Qt UI
    // thread. We collect into a 1-second window and flush a single
    // summary event per window. The timer arm in the select! below
    // forces a flush even when share traffic stops, so the UI doesn't
    // freeze a half-emitted batch when the pool goes quiet after a burst.
    let mut acc_window_count: u64 = 0;
    let mut acc_window_last_seq: u64 = 0;
    let mut acc_window_last_channel: u32 = channel_id;
    let mut acc_window_last_shares_sum: u64 = 0;
    let mut acc_window_started: std::time::Instant = std::time::Instant::now();
    const ACCEPT_FLUSH_MS: u128 = 1000;
    let mut accept_flush_tick =
        tokio::time::interval(std::time::Duration::from_millis(ACCEPT_FLUSH_MS as u64));
    accept_flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
                        // Generation bump invalidates any in-flight hashing
                        // thread immediately (next stale() check returns
                        // true) — replaces the old `cancel` bool flag,
                        // which had a race window where a subsequent
                        // start_hashing_gpu could clear the flag before
                        // the old thread observed it.
                        generation.fetch_add(1, Ordering::SeqCst);
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
                                start_hashing_gpu_shared(
                                    tmpl,
                                    share_target,
                                    gpu.clone(),
                                    args.batch_size,
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
                            tracing::warn!("NewMiningJob without UtreexoStateAnnouncement");
                            continue;
                        };
                        let Some(ctx) = coinbase_ctx.clone() else {
                            tracing::warn!("NewMiningJob without CoinbaseContext");
                            continue;
                        };
                        start_hashing_gpu(
                            tmpl,
                            state,
                            ctx,
                            payout_script.to_vec(),
                            share_target,
                            gpu.clone(),
                            args.batch_size,
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
                            start_hashing_gpu_shared(
                                tmpl,
                                share_target,
                                gpu.clone(),
                                args.batch_size,
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
                        acc_window_count += s.new_submits_accepted_count.max(1) as u64;
                        acc_window_last_seq = s.last_sequence_number as u64;
                        acc_window_last_channel = s.channel_id;
                        acc_window_last_shares_sum = s.new_shares_sum as u64;
                        if acc_window_started.elapsed().as_millis() >= ACCEPT_FLUSH_MS {
                            emitter.emit(
                                "share_accepted",
                                &serde_json::json!({
                                    "channel_id": acc_window_last_channel,
                                    "last_seq": acc_window_last_seq,
                                    "accepted_count": acc_window_count,
                                    "shares_sum": acc_window_last_shares_sum,
                                    "window_ms": acc_window_started.elapsed().as_millis() as u64,
                                }),
                            );
                            acc_window_count = 0;
                            acc_window_started = std::time::Instant::now();
                        }
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
                        share_target = st.max_target;
                        // Force in-flight GPU dispatch thread to exit;
                        // next NewMiningJob will respawn with the new
                        // target captured into the closure.
                        generation.fetch_add(1, Ordering::SeqCst);
                    }
                    other => {
                        tracing::debug!("unhandled frame msg_type=0x{:02x}", other);
                    }
                }
            }
            Some(found) = share_rx.recv() => {
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
                // Only emit share_submitted JSON for block-target hits.
                // Sub-block shares fire ~500/sec at full GPU speed; the
                // per-event UI-thread cost on the Qt side is the dominant
                // cause of frontend freeze + stdout pipe back-pressure
                // that throttled the kernel itself. The pool's
                // SubmitSharesSuccess stream carries the count we need
                // ('share_accepted' events, throttled separately).
                if found.meets_block_target {
                    emitter.emit(
                        "share_submitted",
                        &serde_json::json!({
                            "sequence_number": seq,
                            "nonce": format!("0x{:08x}", found.nonce),
                            "hash": hex::encode(found.hash),
                            "meets_block_target": true,
                            "nonces_searched": found.nonces_searched,
                        }),
                    );
                }
                if found.meets_block_target {
                    blocks_found += 1;
                    if args.max_blocks > 0 && blocks_found >= args.max_blocks {
                        break Ok(blocks_found);
                    }
                }
            }
            _ = accept_flush_tick.tick() => {
                // Periodic flush: if shares accumulated but the next pool
                // ack hasn't arrived, emit what we have so the UI counter
                // tracks the live state instead of freezing on the last
                // burst's tail.
                if acc_window_count > 0
                    && acc_window_started.elapsed().as_millis() >= ACCEPT_FLUSH_MS
                {
                    emitter.emit(
                        "share_accepted",
                        &serde_json::json!({
                            "channel_id": acc_window_last_channel,
                            "last_seq": acc_window_last_seq,
                            "accepted_count": acc_window_count,
                            "shares_sum": acc_window_last_shares_sum,
                            "window_ms": acc_window_started.elapsed().as_millis() as u64,
                        }),
                    );
                    acc_window_count = 0;
                    acc_window_started = std::time::Instant::now();
                }
            }
        }
    };

    reader_task.abort();
    // Bump generation so any GPU dispatch thread still alive from this
    // session observes it and exits before the next session re-spawns.
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
    /// Total hashes searched at this template before the GPU returned —
    /// nonces tried at the current timestamp + (`current_timestamp` -
    /// `tmpl_initial_timestamp`) × 2³² for prior timestamp wraps.
    nonces_searched: u64,
    coinbase_outputs: Vec<CoinbaseOutputWire>,
}

/// GPU mining: assemble miner-owned coinbase + header, dispatch the
/// selected GPU backend's kernel in batches, report found shares via
/// `share_tx`. The backend is chosen at runtime (`--backend`) and passed
/// in as `Arc<dyn GpuBackend>`.
#[allow(clippy::too_many_arguments)]
fn start_hashing_gpu(
    tmpl: NewTemplateDinero,
    pre_block_state: UtreexoAccumulatorState,
    ctx: CoinbaseContext,
    payout_script: Vec<u8>,
    share_target: [u8; 32],
    gpu: Arc<dyn GpuBackend>,
    batch_size: u32,
    generation: Arc<AtomicU64>,
    measured_mhs_x100: Arc<AtomicU64>,
    sampler_state: Arc<SamplerState>,
    share_tx: mpsc::UnboundedSender<FoundShare>,
    emitter: &Emitter,
) {
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
            "backend": gpu.name(),
            "reward_mode": "solo",
        }),
    );

    start_hashing_gpu_template(
        our_template,
        coinbase_outputs_wire,
        share_target,
        gpu,
        batch_size,
        generation,
        measured_mhs_x100,
        sampler_state,
        share_tx,
        emitter.clone(),
    );
}

#[allow(clippy::too_many_arguments)]
fn start_hashing_gpu_shared(
    tmpl: NewTemplateDinero,
    share_target: [u8; 32],
    gpu: Arc<dyn GpuBackend>,
    batch_size: u32,
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
            "backend": gpu.name(),
            "reward_mode": "shared",
        }),
    );

    start_hashing_gpu_template(
        tmpl,
        Vec::new(),
        share_target,
        gpu,
        batch_size,
        generation,
        measured_mhs_x100,
        sampler_state,
        share_tx,
        emitter.clone(),
    );
}

#[allow(clippy::too_many_arguments)]
fn start_hashing_gpu_template(
    tmpl: NewTemplateDinero,
    coinbase_outputs_wire: Vec<CoinbaseOutputWire>,
    share_target: [u8; 32],
    gpu: Arc<dyn GpuBackend>,
    batch_size: u32,
    generation: Arc<AtomicU64>,
    measured_mhs_x100: Arc<AtomicU64>,
    sampler_state: Arc<SamplerState>,
    share_tx: mpsc::UnboundedSender<FoundShare>,
    emitter: Emitter,
) {
    // Each dispatch thread captures its generation. A new tip, target, or
    // template advances the counter and makes all older work stale.
    let gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
    let block_target = nbits_to_target(tmpl.difficulty);

    // Snapshot the job for the FX display's sampler. In non-FX modes this
    // is a cheap, harmless write — nothing ever reads it back.
    *sampler_state.job.lock().unwrap() = Some(tmpl.clone());

    let tmpl_initial_timestamp = tmpl.timestamp;
    let tmpl_version = tmpl.version;
    let tmpl_id = tmpl.template_id;

    // Hash thread:
    // - Inner loop: sweep the full u32 nonce space at the current timestamp.
    // - Outer loop: when the nonce space exhausts, bump timestamp by 1 and
    //   re-sweep. Each timestamp gives a fresh 4.3B-nonce search space.
    //   At ~535 MH/s on M4 Max one sweep takes ~8 s, so we cycle through
    //   timestamps at 1 Hz — well within any pool-side ntime tolerance.
    //   Without this wrap, the thread would silently exit after 8 s of
    //   work and the share counter freezes whenever new pool jobs lag.
    std::thread::spawn(move || {
        let gen_at_spawn = gen;
        let global_generation = Arc::clone(&generation);
        let stale = move || global_generation.load(Ordering::Relaxed) > gen_at_spawn;

        let batch: u64 = batch_size as u64;
        let mut total_ms_since_emit: f64 = 0.0;
        let mut hashes_since_emit: u64 = 0;
        let mut last_emit = std::time::Instant::now();
        const EMIT_INTERVAL_MS: u128 = 1000;

        let mut current_timestamp: u64 = tmpl_initial_timestamp;
        loop {
            if stale() {
                return;
            }
            let header_bytes = assemble_header_bytes(&tmpl, current_timestamp, tmpl_version);
            let mut nonce_start: u64 = 0;
            while nonce_start <= u32::MAX as u64 {
                if stale() {
                    return;
                }
                let this_batch =
                    std::cmp::min(batch, (u32::MAX as u64 + 1).saturating_sub(nonce_start)) as u32;
                let outcome = match gpu.dispatch(
                    &header_bytes,
                    &share_target,
                    nonce_start as u32,
                    this_batch,
                ) {
                    Ok(out) => out,
                    Err(err) => {
                        tracing::error!("gpu dispatch ({}) failed: {err}", gpu.name());
                        return;
                    }
                };
                total_ms_since_emit += outcome.elapsed_ms;
                hashes_since_emit += this_batch as u64;
                sampler_state.nonce_hint.store(
                    ((current_timestamp - tmpl_initial_timestamp) << 32) | nonce_start,
                    Ordering::Relaxed,
                );
                if last_emit.elapsed().as_millis() >= EMIT_INTERVAL_MS {
                    let mhs = (hashes_since_emit as f64 / (total_ms_since_emit / 1000.0)) / 1e6;
                    let dispatch_ms = total_ms_since_emit
                        / ((hashes_since_emit as f64) / (batch_size as f64)).max(1.0);
                    // Publish process-wide rolling rate so the next
                    // session's `nominal_hash_rate_bits` reports actual
                    // measured throughput.
                    measured_mhs_x100.store((mhs * 100.0).round() as u64, Ordering::Relaxed);
                    emitter.emit_gpu_hashrate(
                        mhs,
                        dispatch_ms,
                        nonce_start as u32,
                        current_timestamp,
                        gpu.name(),
                    );
                    last_emit = std::time::Instant::now();
                    total_ms_since_emit = 0.0;
                    hashes_since_emit = 0;
                }
                if outcome.found {
                    let nonce = outcome.nonce;
                    let share = SubmitSharesDinero {
                        channel_id: 0,
                        sequence_number: 0,
                        job_id: 0,
                        nonce,
                        timestamp: current_timestamp,
                        version: tmpl_version,
                    };
                    let hash = HeaderAssembly::hash(&tmpl, &share);
                    if hash < share_target {
                        let meets_block = hash < block_target;
                        // Total nonces searched at this template: completed
                        // wraps × 2³² + nonces tried at the current
                        // timestamp. `hashes_since_emit` was a misleading
                        // proxy that just collapsed to `outcome.nonce`.
                        let timestamp_wraps =
                            current_timestamp.saturating_sub(tmpl_initial_timestamp);
                        let nonces_searched = timestamp_wraps * (1u64 << 32) + outcome.nonce as u64;
                        let _ = share_tx.send(FoundShare {
                            generation: gen,
                            template_id: tmpl_id,
                            timestamp: current_timestamp,
                            version: tmpl_version,
                            nonce,
                            hash,
                            meets_block_target: meets_block,
                            nonces_searched,
                            coinbase_outputs: coinbase_outputs_wire.clone(),
                        });
                    }
                }
                nonce_start += this_batch as u64;
            }
            // Nonce space exhausted at this timestamp. Bump timestamp and
            // continue with a fresh 4.3B-nonce search space.
            current_timestamp = current_timestamp.wrapping_add(1);
            // Sanity bound: don't drift more than ~1 hour past the
            // template-issued timestamp; pool may reject shares that far
            // in the future. Park here until generation flips so the GPU
            // doesn't spin re-sweeping the same exhausted (timestamp,
            // nonce) range; the next NewMiningJob respawns this thread.
            if current_timestamp.saturating_sub(tmpl_initial_timestamp) > 3600 {
                while !stale() {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                return;
            }
        }
    });
}

/// Emit the 128-byte Dinero block header layout (LE) as a byte array so
/// the Metal kernel can hash it per-thread. Nonce bytes at offset 112
/// are set to zero; the kernel overwrites them per thread_position.
fn assemble_header_bytes(tmpl: &NewTemplateDinero, timestamp: u64, version: u32) -> [u8; 128] {
    let mut buf = [0u8; 128];
    buf[0..4].copy_from_slice(&version.to_le_bytes());
    buf[4..36].copy_from_slice(&tmpl.prev_block_hash);
    buf[36..68].copy_from_slice(&tmpl.merkle_root);
    buf[68..100].copy_from_slice(&tmpl.utreexo_root);
    buf[100..108].copy_from_slice(&timestamp.to_le_bytes());
    buf[108..112].copy_from_slice(&tmpl.difficulty.to_le_bytes());
    // buf[112..116] nonce — left zero (kernel writes per-thread)
    // buf[116..128] reserved — left zero (consensus requires all zeros)
    buf
}

// Output modes mirror the CPU miner: `Json` and `Plain` keep today's
// machine formats byte-for-byte; `Human` (TTY stdout, no --json) renders
// the same emit(event, data) calls as Display lines.
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
    fn new(json: bool, human: bool, reward_mode: &str) -> Self {
        let mode = if json {
            OutputMode::Json
        } else if human {
            OutputMode::Human(Arc::new(Mutex::new(HumanState::new(reward_mode))))
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

    fn emit_startup(&self, args: &Args, pool: &str, server_pubkey_pinned: bool) {
        self.emit(
            "startup",
            &serde_json::json!({
                "pool": pool,
                "server_pubkey_pinned": server_pubkey_pinned,
                "batch_size": args.batch_size,
                "user_agent": args.user_agent,
                "version": env!("CARGO_PKG_VERSION"),
                // Requested backend; the resolved one is reported in `gpu_ready`.
                "backend_requested": format!("{:?}", args.backend).to_lowercase(),
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
                "gpu_ready" => {
                    if let Some(b) = data.get("backend").and_then(|v| v.as_str()) {
                        fx.set_backend(b);
                    }
                    fx.lifecycle(&lifecycle_line(event, data));
                }
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
                _ => fx.lifecycle(&lifecycle_line(event, data)),
            },
        }
    }

    /// The GPU hash thread's 1 Hz telemetry line. In Json/Plain mode the
    /// historical hand-rolled line is preserved byte-for-byte (key order
    /// and float formatting — DineroMiner parses this exact shape); human
    /// mode folds it into the status-line repaint instead.
    fn emit_gpu_hashrate(
        &self,
        mhs: f64,
        dispatch_ms: f64,
        nonce_start: u32,
        timestamp: u64,
        backend: &str,
    ) {
        match &self.mode {
            OutputMode::Human(state) => emit_human(
                state,
                "hashrate",
                &serde_json::json!({"mhs": mhs, "backend": backend}),
            ),
            OutputMode::Fx(fx) => {
                fx.set_backend(backend);
                fx.on_hashrate(mhs);
            }
            _ => println!(
                "{{\"event\":\"hashrate\",\"mhs\":{:.2},\"dispatch_ms\":{:.3},\"nonce_start\":\"0x{:08x}\",\"timestamp\":{},\"backend\":\"{}\"}}",
                mhs, dispatch_ms, nonce_start, timestamp, backend,
            ),
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

// ───── Human display (mirrors the CPU miner; GPU adds the backend name
// to the status line and carries reward_mode in state because the GPU
// share_submitted event doesn't include it) ─────

struct HumanState {
    stats: SessionStats,
    last_line_len: usize,
    status_live: bool,
    backend: Option<String>,
    reward_mode: String,
}

impl HumanState {
    fn new(reward_mode: &str) -> Self {
        let stats = SessionStats {
            started: Some(Instant::now()),
            ..Default::default()
        };
        Self {
            stats,
            last_line_len: 0,
            status_live: false,
            backend: None,
            reward_mode: reward_mode.to_string(),
        }
    }

    /// Repaints the status line in place via `\r`, padding with trailing
    /// spaces to the previous line's length so a shorter line doesn't leave
    /// stale glyphs from the longer one behind.
    fn repaint_status(&mut self, out: &mut impl Write) {
        let mut line = Display::status_line(&self.stats);
        if let Some(b) = &self.backend {
            line.push_str("   ");
            line.push_str(b);
        }
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
            if let Some(b) = data.get("backend").and_then(|v| v.as_str()) {
                st.backend = Some(b.to_string());
            }
            st.repaint_status(&mut stdout);
        }
        "gpu_ready" => {
            if let Some(b) = data.get("backend").and_then(|v| v.as_str()) {
                st.backend = Some(b.to_string());
            }
            st.clear_status(&mut stdout);
            println!("{}", lifecycle_line(event, data));
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
                let tries = data
                    .get("nonces_searched")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let mode = st.reward_mode.clone();
                let banner =
                    Display::block_banner(st.stats.blocks, hash, nonce, tries, &mode, &now_hms());
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
        // Behavior change (CLI-miner plan): the clap-level solo default is
        // gone; unspecified yields None and the config resolver's default
        // is shared — same as the CPU miner.
        assert_eq!(Args::try_parse_from(base).unwrap().reward_mode, None);
        assert_eq!(
            resolve_reward_mode("garbage-unknown-mode"),
            RewardModeChoice::Shared
        );

        let shared = base.into_iter().chain(["--reward-mode", "shared"]);
        assert_eq!(
            Args::try_parse_from(shared).unwrap().reward_mode,
            Some(RewardModeChoice::Shared)
        );
    }

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
        assert!(validate_pool_endpoint("no-port-here").is_err());
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
    fn shared_requires_taproot_but_solo_keeps_existing_scripts() {
        let mut p2tr = [0x51u8; 34];
        p2tr[1] = 0x20;
        assert!(validate_reward_payout(RewardModeChoice::Shared, &p2tr).is_ok());
        assert!(validate_reward_payout(RewardModeChoice::Shared, &[0x52, 0x20]).is_err());
        assert!(validate_reward_payout(RewardModeChoice::Solo, &[0x52, 0x20]).is_ok());
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
