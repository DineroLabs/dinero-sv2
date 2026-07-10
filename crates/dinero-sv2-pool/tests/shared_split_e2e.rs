//! Regtest E2E: two shared-mode miners split a found block's coinbase.
//!
//! ## What this test asserts
//!
//! 1. Two miners connect over the real SV2 wire (Noise NX handshake,
//!    `SetupConnection`, `OpenStandardMiningChannel`), then opt into
//!    shared/PPLNS mode via `MSG_SET_REWARD_MODE` with distinct
//!    34-byte taproot payout scripts.
//! 2. They submit accepted standard shares at different rates (miner A
//!    submits more than miner B), so their PPLNS window weights
//!    diverge.
//! 3. One of them grinds a nonce that meets the (trivial regtest)
//!    block target and submits it. The pool assembles the pool-owned
//!    shared coinbase and calls `submitblock` against a real regtest
//!    `dinerod`.
//! 4. The daemon **accepts** the block (it lands in the chain — this
//!    is the actual consensus backstop: multi-output coinbase + v2
//!    utreexo leaves + DNRF-over-all-scripts). We fetch it back via
//!    `getblock` and assert:
//!    - the coinbase pays BOTH miners' payout scripts,
//!    - the higher-weight miner (A) is paid strictly more than B,
//!    - the pool's fee output matches the configured 200 bps,
//!    - the DNRF filter commitment is present (height ≥ 1),
//!    - the sum of all coinbase output values equals the block reward.
//!
//! Run: `DINEROD_BIN=/path/to/dinerod cargo test -p dinero-sv2-pool
//! --test shared_split_e2e -- --ignored --nocapture`
//!
//! ## Harness notes
//!
//! - `RegtestDaemon` below is ported from
//!   `tests/regression_bad_utreexo_root.rs` (spawn/cookie-wait
//!   lifecycle); kept local per this crate's testing convention of not
//!   sharing test-only helpers across integration-test binaries.
//! - The pool binary is located via Cargo's `CARGO_BIN_EXE_<name>` env
//!   var, which Cargo sets automatically for integration tests in a
//!   crate that also builds a `[[bin]]` — no manual path guessing.
//! - Share difficulty is set to the loosest possible
//!   (`--share-leading-bits 0`, vardiff disabled) so **every** hash is
//!   an accepted share; miner A/B are differentiated purely by *share
//!   count*, not by per-share difficulty weight. Whether a given nonce
//!   also happens to be a block-target hit is checked locally so the
//!   accumulation phase can skip those (regtest's block target has a
//!   ~50% per-nonce hit rate, so without this filter almost every
//!   share would also be a premature block).
//! - The PPLNS split is baked into the shared coinbase only at
//!   template-*refresh* time (the pool's producer loop), not at
//!   share-submit time. After accumulating shares, this test actively
//!   waits for a fresh `NewMiningJob` (detected via a changed
//!   `merkle_root`) before grinding for the block, so the winning
//!   block's coinbase actually reflects the accumulated weights.
//! - This test discovered (and this crate now fixes) a real
//!   consensus-mismatch bug: `shared_template.rs` and two call sites
//!   in `main.rs` hardcoded `UTREEXO_MATURITY_LEAF_HEIGHT_MAINNET`
//!   (60_000) with no way to point at regtest's real activation height
//!   (20, confirmed against
//!   `dinero-v8/include/consensus/utreexo_maturity_leaf_activation.h`).
//!   A new `--utreexo-maturity-leaf-height` pool flag (default 60_000,
//!   mainnet-identical) makes this test — and any non-mainnet
//!   deployment — possible. See the task-7 report for detail.

use anyhow::{bail, Context, Result};
use dinero_sv2_codec::{
    decode_new_template, decode_open_standard_mining_channel_success,
    decode_setup_connection_success, decode_submit_shares_error, encode_open_standard_mining_channel,
    encode_setup_connection, encode_submit_shares, sv2::encode_set_reward_mode,
};
use dinero_sv2_common::{
    HeaderAssembly, NewTemplateDinero, OpenStandardMiningChannel, SetRewardMode, SetupConnection,
    SubmitSharesDinero, PROTOCOL_MINING, PROTOCOL_VERSION,
};
use dinero_sv2_pool::{
    mapper,
    rpc::{Auth, RpcClient},
    target::{compact_to_target, hash_meets_target},
};
use dinero_sv2_transport::{
    NoiseSession, MSG_NEW_MINING_JOB, MSG_OPEN_STANDARD_MINING_CHANNEL,
    MSG_OPEN_STANDARD_MINING_CHANNEL_ERROR, MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS,
    MSG_SETUP_CONNECTION, MSG_SETUP_CONNECTION_ERROR, MSG_SETUP_CONNECTION_SUCCESS,
    MSG_SET_REWARD_MODE, MSG_SUBMIT_SHARES_ERROR, MSG_SUBMIT_SHARES_STANDARD,
    MSG_SUBMIT_SHARES_SUCCESS, MSG_WINDOW_STATUS,
};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Regtest dinerod lifecycle (ported from regression_bad_utreexo_root.rs;
// distinct port so the two `--ignored` integration-test binaries can run
// concurrently without colliding).
// ---------------------------------------------------------------------------

struct RegtestDaemon {
    child: Child,
    _datadir: PathBuf,
    rpc_url: String,
    cookie_path: PathBuf,
}

impl RegtestDaemon {
    fn spawn() -> Result<Self> {
        let binary = std::env::var("DINEROD_BIN").unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            format!("{home}/src/dinero/build/dinerod")
        });
        if !Path::new(&binary).is_file() {
            bail!("dinerod binary not found at {binary}; set DINEROD_BIN");
        }

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let datadir = PathBuf::from(format!(
            "/tmp/dinero-sv2-shared-split-e2e-{}-{nanos}",
            std::process::id()
        ));
        if datadir.exists() {
            std::fs::remove_dir_all(&datadir).ok();
        }
        std::fs::create_dir_all(&datadir).context("mkdir datadir")?;

        let port: u16 = 29_979;
        let rpc_url = format!("http://127.0.0.1:{port}");

        let child = Command::new(&binary)
            .arg("--regtest")
            .arg(format!("--datadir={}", datadir.display()))
            .arg("--rpc")
            .arg(format!("--rpcport={port}"))
            .arg("--rpcbind=127.0.0.1")
            .arg("--listen=0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("spawning dinerod")?;

        let cookie_path = datadir.join(".cookie");
        Ok(Self {
            child,
            _datadir: datadir,
            rpc_url,
            cookie_path,
        })
    }

    fn wait_for_cookie(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        while Instant::now() < deadline {
            if self.cookie_path.exists() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        bail!(
            "timed out waiting for cookie at {}",
            self.cookie_path.display()
        )
    }
}

impl Drop for RegtestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Pool process lifecycle
// ---------------------------------------------------------------------------

struct PoolProcess {
    child: Child,
    bind: SocketAddr,
    stderr_log: PathBuf,
}

impl PoolProcess {
    #[allow(clippy::too_many_arguments)]
    fn spawn(
        rpc_url: &str,
        cookie_path: &Path,
        payout_address: &str,
        journal_path: &Path,
        key_path: &Path,
        stderr_log: PathBuf,
    ) -> Result<Self> {
        let binary = env!("CARGO_BIN_EXE_dinero-sv2-pool");
        let bind: SocketAddr = "127.0.0.1:29980".parse()?;
        let stderr_file =
            std::fs::File::create(&stderr_log).context("creating pool stderr log")?;

        let child = Command::new(binary)
            .arg("--bind")
            .arg(bind.to_string())
            .arg("--rpc-url")
            .arg(rpc_url)
            .arg("--cookie")
            .arg(cookie_path.display().to_string())
            .arg("--payout-address")
            .arg(payout_address)
            .arg("--poll-secs")
            .arg("1")
            .arg("--refresh-same-tip-secs")
            .arg("2")
            .arg("--vardiff-target-seconds")
            .arg("0")
            .arg("--share-leading-bits")
            .arg("0")
            .arg("--shared-fee-bps")
            .arg("200")
            .arg("--pplns-journal")
            .arg(journal_path.display().to_string())
            .arg("--tp-key")
            .arg(key_path.display().to_string())
            // The daemon under test (dinero-v8) activates the Utreexo
            // maturity-leaf v2 fork at height 20 on regtest (see
            // UTREEXO_MATURITY_LEAF_HEIGHT_REGTEST in
            // include/consensus/utreexo_maturity_leaf_activation.h),
            // NOT mainnet's 60_000. Without this flag every
            // pool-recomputed utreexo_root past height 20 mismatches
            // the daemon's and submitblock rejects with
            // bad-utreexo-root.
            .arg("--utreexo-maturity-leaf-height")
            .arg("20")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr_file))
            .spawn()
            .context("spawning dinero-sv2-pool")?;

        Ok(Self {
            child,
            bind,
            stderr_log,
        })
    }

    async fn wait_ready(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if TcpStream::connect(self.bind).await.is_ok() {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!(
                    "pool never started listening on {}; stderr tail:\n{}",
                    self.bind,
                    self.tail_log()
                );
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    /// Last ~4000 bytes of the pool's stderr log — used to surface the
    /// real rejection reason (`dinerod rejected our shared block:
    /// <reason>`) when an assertion fails downstream instead of a bare
    /// timeout.
    fn tail_log(&self) -> String {
        match std::fs::read_to_string(&self.stderr_log) {
            Ok(s) => {
                let start = s.len().saturating_sub(4000);
                s[start..].to_string()
            }
            Err(e) => format!("<could not read pool log: {e}>"),
        }
    }
}

impl Drop for PoolProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Minimal SV2 miner client: enough of the wire protocol to open a
// shared-mode channel, grind headers locally, and submit shares.
// ---------------------------------------------------------------------------

struct MinerConn {
    session: NoiseSession<TcpStream>,
    channel_id: u32,
    wire: NewTemplateDinero,
    seq: u32,
}

impl MinerConn {
    async fn connect(pool_addr: SocketAddr, payout_script: Vec<u8>, request_id: u32) -> Result<Self> {
        let tcp = TcpStream::connect(pool_addr)
            .await
            .context("connect to pool")?;
        let mut session = NoiseSession::initiate_nx(tcp, None)
            .await
            .context("noise handshake")?;

        // ---- SetupConnection ----
        let setup = SetupConnection {
            protocol: PROTOCOL_MINING,
            min_version: PROTOCOL_VERSION,
            max_version: PROTOCOL_VERSION,
            flags: 0,
            user_agent: b"shared-split-e2e".to_vec(),
        };
        session
            .write_frame(MSG_SETUP_CONNECTION, &encode_setup_connection(&setup)?)
            .await?;
        let f = session
            .read_frame()
            .await?
            .context("EOF after SetupConnection")?;
        match f.msg_type {
            MSG_SETUP_CONNECTION_SUCCESS => {
                decode_setup_connection_success(&f.payload)?;
            }
            MSG_SETUP_CONNECTION_ERROR => {
                bail!(
                    "SetupConnection.Error: {}",
                    String::from_utf8_lossy(&f.payload)
                )
            }
            other => bail!("unexpected reply to SetupConnection: 0x{other:02x}"),
        }

        // ---- OpenStandardMiningChannel ----
        let open = OpenStandardMiningChannel {
            request_id,
            user_identity: format!("shared-split-e2e-{request_id}").into_bytes(),
            nominal_hash_rate_bits: f32::to_bits(1_000_000.0),
            max_target: [0xFFu8; 32],
        };
        session
            .write_frame(
                MSG_OPEN_STANDARD_MINING_CHANNEL,
                &encode_open_standard_mining_channel(&open)?,
            )
            .await?;
        let f = session
            .read_frame()
            .await?
            .context("EOF after OpenStandardMiningChannel")?;
        let channel_id = match f.msg_type {
            MSG_OPEN_STANDARD_MINING_CHANNEL_SUCCESS => {
                decode_open_standard_mining_channel_success(&f.payload)?.channel_id
            }
            MSG_OPEN_STANDARD_MINING_CHANNEL_ERROR => bail!(
                "OpenStandardMiningChannel.Error: {}",
                String::from_utf8_lossy(&f.payload)
            ),
            other => bail!("unexpected reply to OpenStandardMiningChannel: 0x{other:02x}"),
        };

        // ---- SetRewardMode (shared) ----
        let srm = SetRewardMode {
            channel_id,
            mode: 1,
            payout_script,
        };
        session
            .write_frame(MSG_SET_REWARD_MODE, &encode_set_reward_mode(&srm)?)
            .await?;

        let mut conn = MinerConn {
            session,
            channel_id,
            wire: NewTemplateDinero {
                template_id: 0,
                future_template: false,
                version: 0,
                prev_block_hash: [0u8; 32],
                merkle_root: [0u8; 32],
                utreexo_root: [0u8; 32],
                timestamp: 0,
                difficulty: 0,
                coinbase_outputs_commitment: [0u8; 32],
            },
            seq: 0,
        };
        // `serve_miner` always pushes an initial SOLO job unconditionally
        // right after channel-open (documented pre-Task-6 behaviour,
        // independent of the SetRewardMode we just sent) — so the FIRST
        // `NewMiningJob` frame on this connection is the solo one, with a
        // completely different merkle_root/utreexo_root than the shared
        // template. Waiting for that first NewMiningJob (and only that)
        // would leave `self.wire` pointed at the wrong header — hashes
        // computed against it are uncorrelated with what the pool
        // actually validates shared shares against.
        //
        // `push_shared_job` always sends `SetNewPrevHash` → `NewMiningJob`
        // → `MSG_WINDOW_STATUS`, in that order, and only in shared mode.
        // Waiting for `MSG_WINDOW_STATUS` instead guarantees the most
        // recent `NewMiningJob` we've folded into `self.wire` by the time
        // it arrives is the real shared one.
        conn.wait_for_frame(MSG_WINDOW_STATUS, Duration::from_secs(15))
            .await
            .context("waiting for first shared job (WindowStatus)")?;
        Ok(conn)
    }

    /// Read frames until one of type `want` arrives, updating `self.wire`
    /// on every `NewMiningJob` seen along the way (including the one
    /// returned, if it matches). Frames of other types are drained and
    /// discarded.
    async fn wait_for_frame(&mut self, want: u8, overall_timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + overall_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timed out waiting for frame type 0x{want:02x} on channel {}", self.channel_id);
            }
            let outer = tokio::time::timeout(remaining, self.session.read_frame()).await;
            let frame = match outer {
                Ok(inner) => inner?.ok_or_else(|| anyhow::anyhow!("pool closed connection (EOF)"))?,
                Err(_) => bail!("timed out waiting for frame type 0x{want:02x} on channel {}", self.channel_id),
            };
            if frame.msg_type == MSG_NEW_MINING_JOB {
                self.wire = decode_new_template(&frame.payload)?;
            }
            if frame.msg_type == want {
                return Ok(());
            }
        }
    }

    /// Submit one share and drain frames until we see the matching
    /// success/error ack (any `NewMiningJob`/`WindowStatus`/etc. pushed
    /// in between is drained and, for `NewMiningJob`, folded into
    /// `self.wire`).
    async fn submit_and_ack(&mut self, share: &SubmitSharesDinero, overall_timeout: Duration) -> Result<()> {
        self.seq += 1;
        let mut s = share.clone();
        s.sequence_number = self.seq;
        let payload = encode_submit_shares(&s);
        self.session
            .write_frame(MSG_SUBMIT_SHARES_STANDARD, &payload)
            .await?;

        let deadline = Instant::now() + overall_timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("timed out waiting for share ack on channel {}", self.channel_id);
            }
            let outer = tokio::time::timeout(remaining, self.session.read_frame()).await;
            let frame = match outer {
                Ok(inner) => inner?.ok_or_else(|| anyhow::anyhow!("pool closed connection (EOF)"))?,
                Err(_) => bail!("timed out waiting for share ack on channel {}", self.channel_id),
            };
            if frame.msg_type == MSG_NEW_MINING_JOB {
                self.wire = decode_new_template(&frame.payload)?;
            }
            match frame.msg_type {
                MSG_SUBMIT_SHARES_SUCCESS => return Ok(()),
                MSG_SUBMIT_SHARES_ERROR => {
                    let e = decode_submit_shares_error(&frame.payload)?;
                    bail!(
                        "share rejected: {}",
                        String::from_utf8_lossy(&e.error_code)
                    );
                }
                _ => {}
            }
        }
    }

    /// Submit exactly `count` accepted-but-not-block-worthy shares
    /// against whatever wire template is currently known. Share target
    /// is the pool-wide loosest possible (every hash is accepted — see
    /// `--share-leading-bits 0`), so the only filtering needed here is
    /// against the block target, to avoid accidentally submitting a
    /// premature block during weight accumulation.
    async fn accumulate_shares(&mut self, count: usize) -> Result<()> {
        let mut submitted = 0usize;
        let mut nonce: u32 = 0;
        while submitted < count {
            let block_target = compact_to_target(self.wire.difficulty);
            let share = SubmitSharesDinero {
                channel_id: self.channel_id,
                sequence_number: 0,
                job_id: 0,
                nonce,
                timestamp: self.wire.timestamp,
                version: self.wire.version,
            };
            let hash = HeaderAssembly::hash(&self.wire, &share);
            if !hash_meets_target(&hash, &block_target) {
                self.submit_and_ack(&share, Duration::from_secs(10)).await?;
                submitted += 1;
            }
            nonce = nonce.wrapping_add(1);
            if nonce == 0 {
                bail!("nonce space exhausted accumulating shares (channel {})", self.channel_id);
            }
        }
        Ok(())
    }

    /// Block until a `NewMiningJob` with a different `merkle_root` than
    /// `old_merkle_root` is observed — proof the pool's periodic
    /// template refresh has baked the just-accumulated PPLNS weights
    /// into a fresh shared coinbase (the split only happens at
    /// refresh time, not at share-submit time).
    async fn wait_for_refreshed_job(&mut self, old_merkle_root: [u8; 32], overall_timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + overall_timeout;
        while self.wire.merkle_root == old_merkle_root {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!(
                    "no refreshed shared job (changed merkle_root) within {:?} on channel {}",
                    overall_timeout, self.channel_id
                );
            }
            let slice = remaining.min(Duration::from_secs(1));
            match tokio::time::timeout(slice, self.session.read_frame()).await {
                Ok(inner) => {
                    let frame = inner?.ok_or_else(|| anyhow::anyhow!("pool closed connection (EOF)"))?;
                    if frame.msg_type == MSG_NEW_MINING_JOB {
                        self.wire = decode_new_template(&frame.payload)?;
                    }
                }
                Err(_) => continue,
            }
        }
        Ok(())
    }

    /// Grind nonces against the *current* `self.wire` until one meets
    /// the block target, then submit it.
    async fn find_and_submit_block(&mut self) -> Result<()> {
        let mut nonce: u32 = 0;
        loop {
            let block_target = compact_to_target(self.wire.difficulty);
            let share = SubmitSharesDinero {
                channel_id: self.channel_id,
                sequence_number: 0,
                job_id: 0,
                nonce,
                timestamp: self.wire.timestamp,
                version: self.wire.version,
            };
            let hash = HeaderAssembly::hash(&self.wire, &share);
            if hash_meets_target(&hash, &block_target) {
                self.submit_and_ack(&share, Duration::from_secs(10)).await?;
                return Ok(());
            }
            nonce = nonce.wrapping_add(1);
            if nonce == 0 {
                bail!("nonce space exhausted searching for a block-target hash");
            }
        }
    }
}

/// 34-byte P2TR-shaped scriptPubKey (OP_1 OP_PUSHBYTES_32 <32 bytes>)
/// filled with `fill` — matches the pool's `SetRewardMode` shape check
/// (`len == 34 && [0] == 0x51 && [1] == 0x20`); doesn't need to be a
/// real, spendable taproot output for this test.
fn payout_script(fill: u8) -> Vec<u8> {
    let mut s = vec![0x51, 0x20];
    s.extend(std::iter::repeat(fill).take(32));
    s
}

/// Read a Bitcoin CompactSize varint at `off`. Returns `(value, bytes_consumed)`.
fn read_compact_size(buf: &[u8], off: usize) -> Result<(u64, usize)> {
    if off >= buf.len() {
        bail!("compactsize out of range: off={off} len={}", buf.len());
    }
    let first = buf[off];
    if first < 0xFD {
        Ok((first as u64, 1))
    } else if first == 0xFD {
        if off + 3 > buf.len() {
            bail!("compactsize u16 truncated");
        }
        Ok((u16::from_le_bytes([buf[off + 1], buf[off + 2]]) as u64, 3))
    } else if first == 0xFE {
        if off + 5 > buf.len() {
            bail!("compactsize u32 truncated");
        }
        let v = u32::from_le_bytes([buf[off + 1], buf[off + 2], buf[off + 3], buf[off + 4]]);
        Ok((v as u64, 5))
    } else {
        if off + 9 > buf.len() {
            bail!("compactsize u64 truncated");
        }
        let mut a = [0u8; 8];
        a.copy_from_slice(&buf[off + 1..off + 9]);
        Ok((u64::from_le_bytes(a), 9))
    }
}

/// Parse a coinbase-only block's raw hex (128-byte Dinero header + tx
/// count varint + one segwit-form coinbase tx) into its output list:
/// `(value_una, script_pubkey)` pairs, read directly off the consensus
/// bytes the daemon actually stored (`getblock <hash> 0`) — independent
/// of any RPC convenience/decode layer.
fn parse_coinbase_only_block_outputs(block_hex: &str) -> Result<Vec<(u64, Vec<u8>)>> {
    let bytes = hex::decode(block_hex).context("block hex decode")?;
    if bytes.len() < 128 {
        bail!("block shorter than the 128-byte Dinero header: {} bytes", bytes.len());
    }
    let mut cur = 128usize;
    let (tx_count, n) = read_compact_size(&bytes, cur)?;
    cur += n;
    if tx_count != 1 {
        bail!("expected a coinbase-only block (1 tx), got {tx_count}");
    }
    let tx = &bytes[cur..];

    let mut off = 4usize; // version
    let has_witness = tx.get(off) == Some(&0x00) && tx.get(off + 1) == Some(&0x01);
    if has_witness {
        off += 2;
    }
    let (in_count, n2) = read_compact_size(tx, off)?;
    off += n2;
    for _ in 0..in_count {
        off += 36; // prevout (32 txid + 4 index)
        let (ss_len, n3) = read_compact_size(tx, off)?;
        off += n3 + ss_len as usize;
        off += 4; // sequence
    }
    let (out_count, n4) = read_compact_size(tx, off)?;
    off += n4;
    let mut outputs = Vec::with_capacity(out_count as usize);
    for _ in 0..out_count {
        if off + 8 > tx.len() {
            bail!("output value overflow");
        }
        let mut val = [0u8; 8];
        val.copy_from_slice(&tx[off..off + 8]);
        let value_una = u64::from_le_bytes(val);
        off += 8;
        let (spk_len, n5) = read_compact_size(tx, off)?;
        off += n5;
        if off + spk_len as usize > tx.len() {
            bail!("output scriptPubKey overflow");
        }
        let spk = tx[off..off + spk_len as usize].to_vec();
        off += spk_len as usize;
        outputs.push((value_una, spk));
    }
    Ok(outputs)
}

// ---------------------------------------------------------------------------
// The test itself
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "spawns regtest dinerod + dinero-sv2-pool; run with --ignored"]
async fn shared_block_coinbase_pays_window_contributors() -> Result<()> {
    let started = Instant::now();

    let daemon = RegtestDaemon::spawn().context("spawn regtest dinerod")?;
    daemon.wait_for_cookie().context("wait for cookie")?;
    let rpc = RpcClient::new(
        daemon.rpc_url.clone(),
        Auth::Cookie(daemon.cookie_path.display().to_string()),
    )?;

    // Wallet + payout address for the pool's own getblocktemplate calls.
    let create = rpc
        .call_raw("wallet.createhd", serde_json::json!(["regtestw", "", false]))
        .await
        .context("createhd")?;
    let address = create
        .get("first_address")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("createhd did not return first_address: {create}"))?
        .to_string();

    // Mine well past the regtest Utreexo maturity-leaf v2 activation
    // height (20) so the shared block we're about to build exercises
    // v2 leaves, not the legacy v1 path.
    const SETUP_BLOCKS: u32 = 25;
    let _ = rpc
        .call_raw("generatetoaddress", serde_json::json!([SETUP_BLOCKS, address.clone()]))
        .await
        .context("generatetoaddress (setup)")?;

    // Snapshot the reward + the pool's fee script from a throwaway GBT
    // call before the pool starts touching the mempool/template cycle
    // itself. Since we never mine another block until our own shared
    // block lands, this template describes the identical next-block
    // reward and coinbase shape the pool will build against.
    let gbt = rpc
        .get_block_template(&address)
        .await
        .context("getblocktemplate (pre-pool snapshot)")?;
    let expected_reward_una = gbt
        .get("coinbasevalue")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("missing coinbasevalue in getblocktemplate"))?;
    let coinbase_hex = gbt
        .get("coinbasetxn")
        .and_then(|c| c.get("data"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing coinbasetxn.data in getblocktemplate"))?;
    let fee_script = mapper::extract_fee_script(coinbase_hex).context("extract_fee_script")?;
    let pre_pool_height = gbt
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("missing height in getblocktemplate"))?;

    let tmpdir = std::env::temp_dir().join(format!(
        "dinero-sv2-shared-split-e2e-pool-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&tmpdir).context("mkdir pool tempdir")?;
    let journal_path = tmpdir.join("pplns.jsonl");
    let key_path = tmpdir.join("pool.key");
    let stderr_log = tmpdir.join("pool.stderr.log");

    let pool = PoolProcess::spawn(
        &daemon.rpc_url,
        &daemon.cookie_path,
        &address,
        &journal_path,
        &key_path,
        stderr_log.clone(),
    )
    .context("spawn dinero-sv2-pool")?;
    pool.wait_ready().await.context("pool did not start listening")?;

    let script_a = payout_script(0xA1);
    let script_b = payout_script(0xB2);

    let mut miner_a = MinerConn::connect(pool.bind, script_a.clone(), 1)
        .await
        .context("miner A connect + shared handshake")?;
    let mut miner_b = MinerConn::connect(pool.bind, script_b.clone(), 2)
        .await
        .context("miner B connect + shared handshake")?;

    let pre_accumulation_merkle_root = miner_a.wire.merkle_root;

    // A contributes 3x B's share count → strictly more PPLNS weight
    // (every accepted share carries identical weight here, since both
    // channels share the same, loosest-possible target).
    const SHARES_A: usize = 6;
    const SHARES_B: usize = 2;
    miner_a
        .accumulate_shares(SHARES_A)
        .await
        .context("miner A share accumulation")?;
    miner_b
        .accumulate_shares(SHARES_B)
        .await
        .context("miner B share accumulation")?;

    // The split is only baked into the shared coinbase at the next
    // template refresh (poll-driven, --refresh-same-tip-secs 2 above).
    // Wait for proof it happened rather than guessing with a sleep.
    miner_a
        .wait_for_refreshed_job(pre_accumulation_merkle_root, Duration::from_secs(20))
        .await
        .context("waiting for PPLNS-weighted template refresh")?;

    // Grind + submit the block-worthy share. Doesn't matter which
    // connection does this — the shared coinbase split is entirely
    // window-weight-based, not "whoever finds the block".
    let pre_submit_height: u64 = rpc
        .call_raw("getblockcount", serde_json::json!([]))
        .await?
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("getblockcount: not a u64"))?;
    // `getblockcount` reports the current TIP height; `pre_pool_height`
    // (from getblocktemplate's `height` field) is the height of the NEXT
    // block to be mined — i.e. exactly `pre_submit_height + 1`.
    assert_eq!(pre_submit_height + 1, pre_pool_height, "unexpected chain tip drift before block submission");

    miner_a
        .find_and_submit_block()
        .await
        .context("grind + submit block-target share")?;

    // Poll for the daemon to actually accept the block into the chain.
    let deadline = Instant::now() + Duration::from_secs(20);
    let block_hash = loop {
        let count: u64 = rpc
            .call_raw("getblockcount", serde_json::json!([]))
            .await?
            .as_u64()
            .unwrap_or(pre_submit_height);
        if count > pre_submit_height {
            break rpc
                .call_raw("getbestblockhash", serde_json::json!([]))
                .await?
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("getbestblockhash: not a string"))?
                .to_string();
        }
        if Instant::now() > deadline {
            bail!(
                "dinerod never accepted the shared block (chain tip stuck at {pre_submit_height}). \
                 pool stderr tail:\n{}",
                pool.tail_log()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };

    // ---- Fetch the block back and verify the coinbase split ----
    // `getblock <hash> 1` (the default/JSON form) always returns `tx` as
    // a plain txid-string array regardless of a verbosity argument — no
    // full-transaction-object mode. `wallet.getrawtransaction <txid>
    // true` in turn reported "Transaction not found" for our just-mined
    // coinbase (its chain-db tx-index lookup path, unrelated to this
    // crate). Sidestep both: `getblock <hash> 0` returns the RAW block
    // hex (documented in dinerod's rpc_context_getblock), which we parse
    // ourselves — the strongest possible check, since it reads exactly
    // the consensus bytes the daemon stored, independent of any RPC
    // convenience layer.
    let block_json = rpc
        .call_raw("getblock", serde_json::json!([block_hash, 1]))
        .await
        .context("getblock (json)")?;
    let height = block_json
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("getblock: missing height"))?;
    assert_eq!(height, pre_pool_height, "found block is not the expected next height");
    let n_tx = block_json
        .get("nTx")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("getblock: missing nTx"))?;
    assert_eq!(n_tx, 1, "expected a coinbase-only shared block (Task 5/6 scope)");

    let block_hex = rpc
        .call_raw("getblock", serde_json::json!([block_hash, 0]))
        .await
        .context("getblock (raw hex)")?;
    let block_hex = block_hex
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("getblock verbosity=0: not a string: {block_hex}"))?;
    let outputs = parse_coinbase_only_block_outputs(block_hex)
        .context("parsing coinbase outputs from raw block hex")?;

    let find_value = |script: &[u8]| -> Option<u64> {
        outputs.iter().find(|(_, s)| s == script).map(|(v, _)| *v)
    };

    let value_a = find_value(&script_a).ok_or_else(|| {
        anyhow::anyhow!(
            "coinbase does not pay miner A's script; outputs: {:?}\npool stderr tail:\n{}",
            outputs.iter().map(|(v, s)| format!("{}:{}", hex::encode(s), v)).collect::<Vec<_>>(),
            pool.tail_log()
        )
    })?;
    let value_b = find_value(&script_b).ok_or_else(|| {
        anyhow::anyhow!(
            "coinbase does not pay miner B's script; outputs: {:?}\npool stderr tail:\n{}",
            outputs.iter().map(|(v, s)| format!("{}:{}", hex::encode(s), v)).collect::<Vec<_>>(),
            pool.tail_log()
        )
    })?;
    assert!(
        value_a > value_b,
        "miner A contributed 3x the shares of miner B and must be paid strictly more \
         (A={value_a} una, B={value_b} una)"
    );

    // Stronger than "strictly more": A:B share counts are SHARES_A:
    // SHARES_B == 3:1, and every accepted share carries identical
    // weight here (see the module doc comment), so the split should
    // land A at ~3x B, not just "some amount more". Integer math means
    // compute_split's remainder-handling can fold a few una into the
    // fee/finder output (see its doc comment above), so allow a 1%
    // band around the ideal 3:1 ratio rather than requiring an exact
    // match.
    let expected_a = 3 * value_b;
    let tolerance = expected_a / 100;
    let diff = value_a.abs_diff(expected_a);
    assert!(
        diff <= tolerance,
        "miner A's payout should be ~3x miner B's (share ratio {SHARES_A}:{SHARES_B}); \
         A={value_a} una, B={value_b} una, expected≈{expected_a} una (±{tolerance} una / 1%)"
    );

    // Fee output: 200 bps of the block reward, plus a small (< a few
    // una) rounding remainder the split absorbs into the fee slice —
    // see split::compute_split's remainder-handling doc comment.
    let fee_value = find_value(&fee_script).ok_or_else(|| {
        anyhow::anyhow!("coinbase does not pay the pool's fee script ({})", hex::encode(&fee_script))
    })?;
    let expected_fee_una = (u128::from(expected_reward_una) * 200 / 10_000) as u64;
    assert!(
        fee_value >= expected_fee_una && fee_value <= expected_fee_una + 10,
        "fee output {fee_value} una not within [{expected_fee_una}, {}] (200 bps of {expected_reward_una})",
        expected_fee_una + 10
    );

    // DNRF filter commitment MUST be present (DNRF activates at height
    // 1 on regtest, well below our mined height). DNRW is NOT asserted
    // either way — it's only mandatory from height 10_670, and our low
    // test height legitimately has none (the builder already gates on
    // height).
    const DNRF_MAGIC: [u8; 6] = [0x6a, 0x25, 0x44, 0x4e, 0x52, 0x46];
    assert!(
        outputs.iter().any(|(_, s)| s.starts_with(&DNRF_MAGIC)),
        "coinbase is missing the DNRF filter commitment output"
    );

    // Every value output sums exactly to the block reward.
    let total: u64 = outputs.iter().map(|(v, _)| *v).sum();
    assert_eq!(total, expected_reward_una, "coinbase output sum != block reward");

    eprintln!(
        "shared_block_coinbase_pays_window_contributors: PASS in {:.1}s — block {block_hash} \
         height={height} A={value_a} B={value_b} fee={fee_value} total={total} reward={expected_reward_una}",
        started.elapsed().as_secs_f64()
    );

    Ok(())
}
