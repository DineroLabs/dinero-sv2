//! CUDA compute-backend for SV2 GPU mining on NVIDIA hardware.
//!
//! Same shape as `opencl_backend::OpenClMiner`: one `CudaMiner` owns a device,
//! NVRTC-compiled kernel module, and reused device-side buffers (header,
//! target, result array + count) behind a Mutex; cheap to clone since the
//! inner state is `Arc`-shared. Each `dispatch()` enqueues `batch_size`
//! threads (one nonce per thread) against the embedded sha256d kernel and
//! returns the lowest winning nonce, or "no match".
//!
//! The kernel source (`shaders/sha256d.cu`) is the proven dinero-v8 kernel.
//! It is NVRTC-compiled at `init()` against the active device's compute
//! capability, mirroring the Metal/OpenCL pattern of embedding source via
//! `include_str!`. No build-time `nvcc` invocation is required and the
//! cudarc dynamic-loading variant means no CUDA toolkit at build time
//! either — only the NVIDIA driver + NVRTC runtime when `--backend cuda`
//! actually launches.
//!
//! ## Kernel I/O — result-array, NOT single-winner
//!
//! The CUDA kernel uses a **result array** (`result_nonces[capacity]`,
//! `result_count`) with `atomicAdd`-into-the-array, while OpenCL/Metal use
//! the older single-winner (`result_nonce` + `result_found`) shape. At low
//! share difficulty a 16M-nonce batch can produce many satisfying hashes,
//! and the single-winner shape silently dropped every winner past the first.
//! Here we report every winner up to `RESULT_CAPACITY` and surface overflow
//! to the host (count > capacity). This `dispatch()` returns the **lowest**
//! winning nonce to satisfy the shared `DispatchOutcome` shape — that keeps
//! the host's nonce cursor advancing by the smallest possible amount before
//! the next batch, consistent with how OpenCL/Metal behave today.
//!
//! Tested target: NVIDIA GeForce RTX 4060 (sm_89, Ada Lovelace), CUDA 12.2.

#![cfg(all(feature = "cuda", not(target_os = "macos")))]

use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cudarc::driver::{CudaDevice, CudaFunction, CudaSlice, LaunchAsync, LaunchConfig};
use cudarc::nvrtc::compile_ptx;

use crate::backend::{pack_target_be, DispatchOutcome, GpuBackend};

const KERNEL_SRC: &str = include_str!("../shaders/sha256d.cu");
const KERNEL_NAME: &str = "sha256d_mine";
const MODULE_NAME: &str = "dinero_sha256d";

/// Block size for the kernel launch. SHA-256d is register-bound, so 256
/// threads/block hits the sweet spot on Ada/Ampere (matches the dinero-v8
/// host launch config). The kernel returns early past `batch_size` so a
/// grid rounded up to a whole block never hashes outside the requested range.
const THREADS_PER_BLOCK: u32 = 256;

/// Capacity of the per-dispatch result_nonces array. A 16M-nonce batch at
/// share difficulty 1 (every hash satisfies) would overflow far below 256,
/// so this is overkill for realistic shares; the kernel still reports
/// `result_count > capacity` so an operator sees the overflow in logs.
const RESULT_CAPACITY: u32 = 256;

#[derive(Clone)]
pub struct CudaMiner {
    inner: Arc<Inner>,
}

struct Inner {
    device: Arc<CudaDevice>,
    function: CudaFunction,
    device_name: String,
    max_threads_per_group: u64,
    // Reused per dispatch; mutex serialises since each dispatch writes
    // inputs, launches, and reads outputs. One miner thread is the steady
    // state in `start_hashing_gpu`.
    buffers: Mutex<DispatchBuffers>,
}

struct DispatchBuffers {
    header: CudaSlice<u8>,         // 128 bytes — kernel reinterprets as 32 LE u32 words
    target: CudaSlice<u32>,        //   8 × u32 BE words
    result_nonces: CudaSlice<u32>, //   RESULT_CAPACITY × u32 (each = winning nonce)
    result_count: CudaSlice<u32>,  //   1 × u32 (total satisfying nonces, may exceed capacity)
}

impl CudaMiner {
    pub fn init() -> Result<Self> {
        // GPU 0. Multi-GPU fan-out is a follow-up — would partition the nonce
        // range across one CudaMiner per device sharing the SV2 session.
        let device = CudaDevice::new(0).context(
            "no CUDA device available — install the NVIDIA driver + CUDA runtime, \
             or use --backend opencl / the CPU miner dinero-sv2-miner",
        )?;

        let device_name = device
            .name()
            .unwrap_or_else(|_| "unknown CUDA device".to_string());

        // NVRTC-compile the embedded kernel at startup. The driver caches PTX
        // across processes so subsequent runs only recompile if the source
        // changes (it doesn't, after a release build embeds the .cu).
        let ptx = compile_ptx(KERNEL_SRC).context("nvrtc compile sha256d.cu")?;
        device
            .load_ptx(ptx, MODULE_NAME, &[KERNEL_NAME])
            .context("cuda load_ptx(sha256d)")?;
        let function = device
            .get_func(MODULE_NAME, KERNEL_NAME)
            .context("cuda get_func(sha256d_mine)")?;

        // Reported in the gpu_ready event. sm_89 (Ada) reports 1024; older
        // architectures report less. The launch itself uses THREADS_PER_BLOCK
        // (256) regardless — this is just for the event/log.
        let max_threads_per_group: u64 = 1024;

        let header = device.alloc_zeros::<u8>(128).context("alloc header")?;
        let target = device.alloc_zeros::<u32>(8).context("alloc target")?;
        let result_nonces = device
            .alloc_zeros::<u32>(RESULT_CAPACITY as usize)
            .context("alloc result_nonces")?;
        let result_count = device.alloc_zeros::<u32>(1).context("alloc result_count")?;

        Ok(CudaMiner {
            inner: Arc::new(Inner {
                device,
                function,
                device_name,
                max_threads_per_group,
                buffers: Mutex::new(DispatchBuffers {
                    header,
                    target,
                    result_nonces,
                    result_count,
                }),
            }),
        })
    }

    pub fn dispatch(
        &self,
        header_bytes: &[u8; 128],
        target: &[u8; 32],
        nonce_start: u32,
        batch_size: u32,
    ) -> Result<DispatchOutcome> {
        let inner = &self.inner;
        let mut guard = inner.buffers.lock().expect("cuda buffers mutex");
        // Reborrow to a plain &mut so disjoint-field borrows split — the
        // launch tuple holds shared borrows on header/target alongside
        // exclusive borrows on result_*, which the borrow-checker only
        // accepts on a direct reference, not through MutexGuard.
        let bufs: &mut DispatchBuffers = &mut *guard;

        // Target packs to 8 BE u32 words. Kernel's hash_meets_target walks
        // [7]→[0] (MSW first) against the SHA-256 BE state — same convention
        // as Metal/OpenCL (`backend::pack_target_be` is the single source).
        let target_words = pack_target_be(target);

        inner
            .device
            .htod_sync_copy_into(header_bytes, &mut bufs.header)
            .context("upload header")?;
        inner
            .device
            .htod_sync_copy_into(&target_words, &mut bufs.target)
            .context("upload target")?;
        inner
            .device
            .htod_sync_copy_into(&[0u32; 1], &mut bufs.result_count)
            .context("reset result_count")?;
        // result_nonces is overwritten only at the slots the kernel uses
        // (driven by atomicAdd into result_count), and the host reads at
        // most `count` of them — stale values past count are never observed.

        let grid = batch_size.div_ceil(THREADS_PER_BLOCK);
        let cfg = LaunchConfig {
            grid_dim: (grid, 1, 1),
            block_dim: (THREADS_PER_BLOCK, 1, 1),
            shared_mem_bytes: 0,
        };

        let t0 = Instant::now();
        // SAFETY: kernel signature matches the args tuple — see the
        // `sha256d_mine` parameter list in shaders/sha256d.cu. Inputs are
        // shared-borrowed, outputs are exclusive-borrowed.
        unsafe {
            inner.function.clone().launch(
                cfg,
                (
                    &bufs.header,
                    &bufs.target,
                    nonce_start,
                    batch_size,
                    &mut bufs.result_nonces,
                    &mut bufs.result_count,
                    RESULT_CAPACITY,
                ),
            )
        }
        .context("launch sha256d_mine")?;

        inner.device.synchronize().context("cuda sync after launch")?;
        let elapsed = t0.elapsed();

        let count_vec = inner
            .device
            .dtoh_sync_copy(&bufs.result_count)
            .context("read result_count")?;
        let total_count = count_vec[0];
        let elapsed_ms = elapsed.as_secs_f64() * 1000.0;

        if total_count == 0 {
            return Ok(DispatchOutcome {
                found: false,
                nonce: 0,
                elapsed_ms,
            });
        }

        // total_count may exceed RESULT_CAPACITY when the share is so loose
        // it produces more winners than the array can hold. The host can
        // still pick a winner from the first `capacity` slots; the overflow
        // is logged so an operator notices misconfigured share difficulty.
        let visible = total_count.min(RESULT_CAPACITY) as usize;
        if total_count > RESULT_CAPACITY {
            tracing::warn!(
                total = total_count,
                capacity = RESULT_CAPACITY,
                "CUDA dispatch produced more winners than result_nonces capacity; \
                 share target may be too loose"
            );
        }

        let nonces = inner
            .device
            .dtoh_sync_copy(&bufs.result_nonces)
            .context("read result_nonces")?;
        let lowest = nonces[..visible].iter().copied().min().unwrap_or(0);

        Ok(DispatchOutcome {
            found: true,
            nonce: lowest,
            elapsed_ms,
        })
    }
}

impl GpuBackend for CudaMiner {
    fn name(&self) -> &'static str {
        "cuda"
    }

    fn device_name(&self) -> &str {
        &self.inner.device_name
    }

    fn max_threads_per_group(&self) -> u64 {
        self.inner.max_threads_per_group
    }

    fn dispatch(
        &self,
        header_bytes: &[u8; 128],
        target: &[u8; 32],
        nonce_start: u32,
        batch_size: u32,
    ) -> Result<DispatchOutcome> {
        // Inherent `CudaMiner::dispatch` takes priority in method resolution
        // (so this delegates to it rather than recursing).
        self.dispatch(header_bytes, target, nonce_start, batch_size)
    }
}

// Kernel parity test — must reach an actual NVIDIA GPU + CUDA driver to run,
// so it is `#[ignore]` by default. Run on the NVIDIA host with:
//   cargo test --release -p dinero-sv2-gpu-miner --features cuda \
//     -- --ignored cuda_parity
//
// Lives in-crate (NOT under `tests/`) because this crate is `[[bin]]`-only —
// an integration test cannot reach `backend::pack_target_be` etc.
#[cfg(test)]
mod cuda_parity_tests {
    use super::*;

    /// CPU double-SHA256 of the full 128-byte BlockHeader v1 form with a
    /// 32-bit nonce inserted at byte offset 112 — exactly what the kernel
    /// hashes per thread. Returns the SHA-256d hash as 32 big-endian bytes.
    fn sha256d_128_with_nonce(template128: &[u8; 128], nonce: u32) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut buf = *template128;
        buf[112..116].copy_from_slice(&nonce.to_le_bytes());
        let first = Sha256::digest(&buf[..]);
        let second = Sha256::digest(first);
        let mut out = [0u8; 32];
        out.copy_from_slice(&second);
        out
    }

    /// Big-endian compare of a 32-byte hash against a 32-byte target.
    /// hash[0] is the MSB; `true` iff hash <= target.
    fn hash_meets_target(hash: &[u8; 32], target: &[u8; 32]) -> bool {
        for i in 0..32 {
            if hash[i] < target[i] {
                return true;
            }
            if hash[i] > target[i] {
                return false;
            }
        }
        true
    }

    #[test]
    #[ignore = "requires an NVIDIA GPU + CUDA driver"]
    fn cuda_parity_lowest_nonce_matches_cpu_reference() {
        // Deterministic header (all zeros at most positions; nonce-only fuzz).
        // A tiny batch keeps the CPU sweep cheap; the loose 8-leading-zero-byte
        // target produces a handful of winners under SHA-256d, so we exercise
        // both the "found" path and the "lowest of many" tie-break.
        let mut header = [0u8; 128];
        // Differentiate the header so a CPU collision against another test's
        // header is implausible. Set a few non-zero bytes outside the nonce
        // window (76..80 is the legacy nonce slot; 112..116 is the v1 slot).
        for (i, b) in header.iter_mut().enumerate().take(72) {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }

        let mut target = [0xffu8; 32];
        // Loose target: 8 leading zero bits in BE → ~1 in 256 nonces wins.
        target[0] = 0x00;

        let nonce_start: u32 = 0;
        let batch_size: u32 = 4096; // ~16 winners expected on average.

        let miner = CudaMiner::init().expect("CudaMiner init (requires CUDA)");
        let outcome = miner
            .dispatch(&header, &target, nonce_start, batch_size)
            .expect("cuda dispatch");

        // CPU sweep the same range to compute the expected lowest winner.
        let cpu_lowest = (nonce_start..nonce_start + batch_size)
            .find(|n| {
                let h = sha256d_128_with_nonce(&header, *n);
                hash_meets_target(&h, &target)
            });

        match (outcome.found, cpu_lowest) {
            (false, None) => { /* both agree: no winner in range */ }
            (true, Some(expected)) => {
                assert_eq!(
                    outcome.nonce, expected,
                    "CUDA returned nonce {:#x}, CPU lowest winner was {:#x}",
                    outcome.nonce, expected
                );
                // Re-verify CUDA's claimed nonce satisfies the target via CPU
                // sha256d — guards against a kernel bug producing a "winner"
                // that doesn't actually beat the target.
                let hash = sha256d_128_with_nonce(&header, outcome.nonce);
                assert!(
                    hash_meets_target(&hash, &target),
                    "CUDA returned nonce {:#x} but CPU hash {:?} does not meet target",
                    outcome.nonce,
                    hash
                );
            }
            (true, None) => panic!(
                "CUDA reports found nonce {:#x} but CPU finds no winner in range",
                outcome.nonce
            ),
            (false, Some(n)) => panic!(
                "CUDA reports no winner but CPU found one at nonce {:#x}",
                n
            ),
        }
    }
}
