//! CUDA compute-backend for SV2 GPU mining on NVIDIA — SCAFFOLD ONLY.
//!
//! The kernel (`shaders/sha256d.cu`, a faithful carry-over of the tested
//! dinero-qt/dinero-v8 kernel) is embedded and ready. The cudarc + NVRTC
//! runtime integration is intentionally deferred to a Linux/NVIDIA host where
//! `cargo build --features cuda` can actually compile and run it — CUDA does
//! not exist on Apple Silicon, so writing the cudarc body here could not be
//! compiled and would be unverifiable guesswork.
//!
//! Until that host work lands (Task 4: cudarc context + NVRTC compile +
//! result-array buffers `result_nonces[capacity]` / `result_count` mirroring
//! the kernel's contract), `init()` returns an actionable error. The effect:
//! `--backend cuda` reports CUDA as unavailable, and `--backend auto` silently
//! falls back to OpenCL — exactly the runtime contract in the spec.

#![cfg(feature = "cuda")]
// The whole struct is a deliberate stub until the cudarc body lands; on macOS
// the (non-macOS) build_backend CUDA arm is compiled out, so nothing
// constructs it. Suppress the resulting dead-code noise.
#![allow(dead_code)]

use anyhow::Result;

use crate::backend::{DispatchOutcome, GpuBackend};

/// Embedded CUDA-C kernel — NVRTC-compiled at `init()` once the cudarc body
/// is implemented (Task 4, on the NVIDIA host).
const KERNEL_SRC: &str = include_str!("../shaders/sha256d.cu");
const KERNEL_ENTRY: &str = "sha256d_mine";

pub struct CudaMiner {
    device_name: String,
}

impl CudaMiner {
    /// Scaffold: always errors. The real implementation (cudarc Driver API +
    /// NVRTC compile of `KERNEL_SRC` + first-GPU select) lands on the NVIDIA
    /// host. The error is actionable so operators understand the state.
    pub fn init() -> Result<Self> {
        anyhow::bail!(
            "CUDA backend is scaffolded but not yet wired to cudarc/NVRTC in \
             this build (kernel embedded: {} bytes, entry `{}`). Build \
             --features cuda on a Linux/NVIDIA host once cuda_backend.rs is \
             implemented, or use --backend opencl / the CPU miner.",
            KERNEL_SRC.len(),
            KERNEL_ENTRY,
        )
    }
}

impl GpuBackend for CudaMiner {
    fn name(&self) -> &'static str {
        "cuda"
    }

    fn device_name(&self) -> &str {
        &self.device_name
    }

    fn max_threads_per_group(&self) -> u64 {
        // Unreachable: `CudaMiner` is never constructed until `init()` is real.
        unreachable!("CUDA backend not implemented")
    }

    fn dispatch(
        &self,
        _header_bytes: &[u8; 128],
        _target: &[u8; 32],
        _nonce_start: u32,
        _batch_size: u32,
    ) -> Result<DispatchOutcome> {
        unreachable!("CUDA backend not implemented")
    }
}
