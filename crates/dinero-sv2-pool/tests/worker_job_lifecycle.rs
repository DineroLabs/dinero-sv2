//! Real worker protocol regressions. The fake pool deliberately separates
//! new-tip, job and target frames; an ordinary pool's back-to-back frames
//! hide bugs in cancellation and vardiff recovery. No daemon is needed.
use anyhow::{bail, ensure, Context, Result};
use dinero_sv2_codec::{sv2::*, *};
use dinero_sv2_common::*;
use dinero_sv2_transport::*;
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct ReaderTask(tokio::task::JoinHandle<()>);
impl Drop for ReaderTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn event(log: &PathBuf, name: &str, key: &str, value: serde_json::Value) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let data = std::fs::read_to_string(log)?;
        if data
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .any(|v| v["event"] == name && v[key] == value)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    bail!(
        "worker did not acknowledge {name} {key}={value}; logs: {}",
        log.display()
    )
}

async fn share(
    rx: &mut mpsc::UnboundedReceiver<Frame>,
    job: &NewTemplateDinero,
    target: [u8; 32],
) -> Result<()> {
    let frame = tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .context("worker stopped producing shares without a replacement job")?
        .context("worker disconnected")?;
    ensure!(
        frame.msg_type == MSG_SUBMIT_SHARES_STANDARD,
        "unexpected worker frame"
    );
    let s = decode_submit_shares(&frame.payload)?;
    ensure!(
        s.channel_id == 7 && s.job_id as u64 == job.template_id,
        "stale or wrong-channel share: {s:?}"
    );
    ensure!(
        s.timestamp >= job.timestamp && s.timestamp < job.timestamp + 120,
        "worker did not preserve the job's 64-bit minimum timestamp: {s:?}"
    );
    ensure!(s.version == job.version, "worker changed version");
    let hash = HeaderAssembly::hash(job, &s);
    ensure!(hash < target, "worker submitted hash above the job target");
    Ok(())
}

async fn quiet(rx: &mut mpsc::UnboundedReceiver<Frame>) -> Result<()> {
    // Allow frames written before the acknowledged invalidation to drain.
    // The peer reader stays alive: cancelling Noise read_frame mid-frame can
    // corrupt cipher state, so only the mpsc receive is ever timed out.
    tokio::time::sleep(Duration::from_millis(250)).await;
    while rx.try_recv().is_ok() {}
    match tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
        Err(_) => Ok(()),
        Ok(Some(f)) => bail!(
            "worker submitted work after acknowledged invalidation: {:?}",
            decode_submit_shares(&f.payload)
        ),
        Ok(None) => bail!("worker disconnected instead of waiting for work"),
    }
}

#[tokio::test]
#[ignore = "requires DINEROMINER_BIN pointing to the actual CPU worker"]
async fn cpu_job_lifecycle() -> Result<()> {
    run(false).await
}

#[tokio::test]
#[ignore = "requires DINEROGPUMINER_BIN and a physical GPU"]
async fn gpu_job_lifecycle() -> Result<()> {
    run(true).await
}

async fn run(gpu: bool) -> Result<()> {
    let evidence = tempfile::Builder::new()
        .prefix("dinero-worker-lifecycle-")
        .tempdir()?
        .keep();
    eprintln!("worker evidence: {}", evidence.display());
    let log = evidence.join("events.jsonl");
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let keys = StaticKeys::generate()?;
    let binary = std::env::var(if gpu {
        "DINEROGPUMINER_BIN"
    } else {
        "DINEROMINER_BIN"
    })?;
    let mut cmd = Command::new(binary);
    cmd.args([
        "--pool",
        &listener.local_addr()?.to_string(),
        "--server-pubkey",
        &keys.public_hex(),
        "--reward-mode",
        "shared",
        "--no-save",
        "--json",
        "--payout-script-hex",
        &format!("5120{}", "ab".repeat(32)),
    ]);
    if gpu {
        cmd.args(["--batch-size", "4096"]);
    } else {
        cmd.args(["--threads", "1"]);
    }
    let _worker = Worker(
        cmd.stdin(Stdio::null())
            .stdout(std::fs::File::create(&log)?)
            .stderr(std::fs::File::create(evidence.join("stderr.log"))?)
            .spawn()?,
    );
    let (tcp, _) = tokio::time::timeout(Duration::from_secs(20), listener.accept()).await??;
    let session = NoiseSession::accept_nx(tcp, &keys).await?;
    let (mut reader, mut writer) = session.split();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let _reader = ReaderTask(tokio::spawn(async move {
        while let Ok(Some(f)) = reader.read_frame().await {
            if tx.send(f).is_err() {
                break;
            }
        }
    }));
    let setup = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await?
        .context("setup EOF")?;
    ensure!(setup.msg_type == MSG_SETUP_CONNECTION, "setup missing");
    writer
        .write_frame(
            MSG_SETUP_CONNECTION_SUCCESS,
            &encode_setup_connection_success(&SetupConnectionSuccess {
                used_version: PROTOCOL_VERSION,
                flags: 0,
            }),
        )
        .await?;
    let open = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await?
        .context("open EOF")?;
    ensure!(
        open.msg_type == MSG_OPEN_STANDARD_MINING_CHANNEL,
        "open missing"
    );
    let request_id = decode_open_standard_mining_channel(&open.payload)?.request_id;
    let mut target = [0xff; 32];
    target[..2].fill(0);
    target[2] = 0x3f;
    writer
        .write_frame(
            MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
            &encode_open_standard_mining_channel_success(&OpenStandardMiningChannelSuccess {
                request_id,
                channel_id: 7,
                target,
            }),
        )
        .await?;
    let mode = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await?
        .context("mode EOF")?;
    ensure!(
        mode.msg_type == MSG_SET_REWARD_MODE,
        "shared mode not requested"
    );
    writer
        .write_frame(
            MSG_WINDOW_STATUS,
            &encode_window_status(&WindowStatus {
                channel_id: 7,
                window_bps: 10000,
                window_shares: 1,
            })?,
        )
        .await?;
    // Above u32 verifies that neither worker truncates Dinero's timestamp.
    let mut job = NewTemplateDinero {
        template_id: 10,
        future_template: false,
        version: 0x20000000,
        prev_block_hash: [1; 32],
        merkle_root: [2; 32],
        utreexo_root: [3; 32],
        timestamp: (1_u64 << 32) + 100,
        difficulty: 0x03000001,
        coinbase_outputs_commitment: [0; 32],
    };
    send_job(&mut writer, &job).await?;
    for _ in 0..3 {
        share(&mut rx, &job, target).await?;
    }
    writer
        .write_frame(
            MSG_SET_NEW_PREV_HASH,
            &encode_set_new_prev_hash(&SetNewPrevHash {
                channel_id: 7,
                prev_hash: [4; 32],
                min_ntime: job.timestamp + 60,
                nbits: 0x03000002,
            }),
        )
        .await?;
    // Ordered WindowStatus is a barrier: the miner has consumed SNPH before
    // emitting this event, so a scheduler delay cannot fake the regression.
    writer
        .write_frame(
            MSG_WINDOW_STATUS,
            &encode_window_status(&WindowStatus {
                channel_id: 7,
                window_bps: 10000,
                window_shares: 999,
            })?,
        )
        .await?;
    event(
        &log,
        "window_status",
        "window_shares",
        serde_json::json!(999),
    )
    .await?;
    quiet(&mut rx).await?;
    writer
        .write_frame(
            MSG_SET_TARGET,
            &encode_set_target(&SetTarget {
                channel_id: 7,
                max_target: target,
            }),
        )
        .await?;
    event(
        &log,
        "set_target",
        "max_target",
        serde_json::json!(hex::encode(target)),
    )
    .await?;
    quiet(&mut rx).await?;
    eprintln!("PASS: no old-job shares while the next job is withheld");
    job.template_id += 1;
    job.prev_block_hash = [4; 32];
    job.timestamp += 60;
    job.difficulty = 0x03000002;
    writer
        .write_frame(MSG_NEW_MINING_JOB, &encode_new_template(&job))
        .await?;
    for _ in 0..3 {
        share(&mut rx, &job, target).await?;
    }
    writer
        .write_frame(
            MSG_SET_TARGET,
            &encode_set_target(&SetTarget {
                channel_id: 7,
                max_target: [0; 32],
            }),
        )
        .await?;
    event(
        &log,
        "set_target",
        "max_target",
        serde_json::json!("00".repeat(32)),
    )
    .await?;
    quiet(&mut rx).await?;
    writer
        .write_frame(
            MSG_SET_TARGET,
            &encode_set_target(&SetTarget {
                channel_id: 7,
                max_target: target,
            }),
        )
        .await?;
    // No NewMiningJob: vardiff must resume the current valid job itself.
    for _ in 0..3 {
        share(&mut rx, &job, target).await?;
    }
    eprintln!("PASS: target-only update resumed valid work; 64-bit timestamps and hashes verified");
    Ok(())
}

async fn send_job(
    writer: &mut NoiseWriter<tokio::io::WriteHalf<TcpStream>>,
    job: &NewTemplateDinero,
) -> Result<()> {
    writer
        .write_frame(
            MSG_SET_NEW_PREV_HASH,
            &encode_set_new_prev_hash(&SetNewPrevHash {
                channel_id: 7,
                prev_hash: job.prev_block_hash,
                min_ntime: job.timestamp,
                nbits: job.difficulty,
            }),
        )
        .await?;
    writer
        .write_frame(MSG_NEW_MINING_JOB, &encode_new_template(job))
        .await?;
    Ok(())
}
