# Spec — Native CUDA backend for `dinero-sv2-gpu-miner`

Date: 2026-06-06
Status: approved (with edits 2026-06-06)
Repo: `DineroLabs/dinero-sv2`

## Goal

The SV2 GPU pool miner currently compiles **one** GPU backend per OS — Metal on macOS, OpenCL elsewhere — selected at compile time. NVIDIA GPUs run via OpenCL: functional, but *not* the native CUDA path the dinero-qt **solo** miner already ships and has tested. Add a native CUDA backend so SV2 pool GPU mining reaches **Metal + OpenCL + CUDA** parity with solo.

Product outcome: *"Solo or pool, every major GPU backend works natively."*

## Non-goals (YAGNI)

- No multi-GPU fan-out (a follow-up, same as OpenCL today — both pick the first GPU).
- No changes to the SV2 protocol layer, the CPU miner (`dinero-sv2-miner`), or dinero-qt. (The dinero-qt Solo/Pool UI integration is a separate sub-project, sequenced after this.)
- No new hashing algorithm — reuse the existing, tested sha256d midstate + nonce-search kernel.
- DineroDPI phone mining stays CPU/dev/contribute only — explicitly out of scope.

## Shared backend abstraction (refactor before adding CUDA)

Today `MetalMiner` and `OpenClMiner` each define their *own* `DispatchOutcome` and are selected by a compile-time `type GpuMiner = …` alias. Runtime `--backend` needs one shared shape. Introduce a single abstraction (trait **or** enum — whichever is simpler in `main.rs`):

```rust
pub struct DispatchOutcome { pub found: bool, pub nonce: u32, pub elapsed_ms: f64 }

pub trait GpuBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn device_name(&self) -> &str;
    fn dispatch(&self, header: &[u8; 128], target: &[u8; 32],
                start_nonce: u32, batch_size: u32) -> Result<DispatchOutcome>;
}
```

- **The public `dispatch` interface takes `target: &[u8; 32]` (raw bytes)** — matching the existing Metal/OpenCL surface, to avoid needless churn. Each backend internally packs it into 8 big-endian `u32` words (Metal/OpenCL already do this).
- Refactor `MetalMiner`/`OpenClMiner` onto the shared `DispatchOutcome` + this interface (behavior unchanged).
- `init() -> Result<Self>` stays per-backend.

## CUDA backend

A new `CudaMiner` implementing `GpuBackend`:

- `init()` — select the first CUDA GPU; clean, actionable error if no driver/device.
- `dispatch(header[128], target[32], start_nonce, batch_size)` — pack target → 8 BE u32 (same convention as OpenCL), launch the kernel over `batch_size` nonces from `start_nonce`, copy results back. `Send + Sync`, mutex-serialized reused buffers, exactly like `OpenClMiner`.

### Crate + kernel

- **`cudarc`** — pure-Rust CUDA Driver API + NVRTC bindings that **dynamically load the system CUDA driver/NVRTC at runtime**. Self-contained Rust miner; no binding to the C++ miner.
- **Kernel:** port `dinero-v8/miner/cmake/sha256d_cu_src.cpp.in` (the tested CUDA-C sha256d kernel) into the crate (`shaders/sha256d.cu`, embedded), **NVRTC-compiled at `init()`** — mirroring how `opencl_backend` compiles `sha256d.cl`. One thread per nonce; write `result_found`/`result_nonce` on hash ≤ target.

### cudarc runtime contract (explicit)

- **Normal (non-CUDA) workspace builds require NO CUDA toolkit** at build time.
- The CUDA backend requires an **NVIDIA driver + NVRTC runtime present when `--backend cuda` is actually used**.
- **Explicit `--backend cuda` fails with a clear, actionable error** if the driver/NVRTC is unavailable ("install the NVIDIA driver + CUDA runtime, or use `--backend opencl` / the CPU miner").
- **`--backend auto` silently skips CUDA** and falls back to OpenCL when CUDA is unavailable.

## Backend selection

- Add `--backend <auto|metal|opencl|cuda>` (mirrors the solo miner's `MinerBackend`).
- `auto`: macOS → Metal; otherwise try CUDA (`CudaMiner::init()` succeeds ⇒ a usable CUDA GPU) → fall back to OpenCL.
- Explicit value forces that backend and hard-errors if unavailable.
- Replace the compile-time `type GpuMiner = …` alias with runtime dispatch over the shared abstraction so `start_hashing_gpu` stays backend-agnostic.

## Cargo feature gating

- An **optional `cuda` feature** (NOT default-on). It pulls in `cudarc` and compiles `cuda_backend`. Enabled by the **Linux release/package builds**; absent from the default dev/test profile. Acceptable alternative: a `cfg(target_os = "linux"/"windows")`-gated dependency + compile guard — but only after proving the crate builds on a no-CUDA host.
- **`cargo build`/`cargo test --workspace` with default features MUST NOT require CUDA** (no toolkit, no driver). CUDA code is behind the feature/guard.

## Error handling

- `init()` → clean actionable error if no CUDA device/driver (fatal under explicit `--backend cuda`; swallowed → OpenCL under `auto`).
- NVRTC compile failures surface the compiler log.
- Per-dispatch CUDA errors propagate as `Result` into the existing miner loop (already handles backend errors → reconnect/abort).

## Testing

**Default CI (no GPU, blocking):**
- The crate **builds with default features (no CUDA)**, and with the `cuda` feature on a host *without* a driver still compiles (dynamic-load; CUDA only touched at runtime).
- **Backend-agnostic correctness:** the BE-u32 target packing + a CPU sha256d reference are exercised in unit tests (no GPU needed) — these pin the cross-backend contract.

**CUDA host (manual / GPU-CI, non-blocking):**
- **Kernel parity:** for a fixed header+target, CUDA's sha256d and first-valid nonce equal the CPU reference and the OpenCL/Metal backends.
- **Real pool smoke:** `dinero-sv2-gpu-miner --backend cuda --pool <SJ 173.249.200.59:4444> --server-pubkey bcaa90… --payout-script-hex …` → assert `channel_open → new_job → share_accepted` (same shape as the CPU E2E done 2026-06-06).

## Files touched

- `crates/dinero-sv2-gpu-miner/src/backend.rs` (new) — shared `GpuBackend` + `DispatchOutcome`.
- `crates/dinero-sv2-gpu-miner/src/{metal_backend.rs,opencl_backend.rs}` — refactor onto the shared shape (no behavior change).
- `crates/dinero-sv2-gpu-miner/src/cuda_backend.rs` (new) — `CudaMiner`.
- `crates/dinero-sv2-gpu-miner/shaders/sha256d.cu` (new) — kernel ported from dinero-qt's `sha256d_cu`.
- `crates/dinero-sv2-gpu-miner/src/main.rs` — runtime backend dispatch + `--backend` flag.
- `crates/dinero-sv2-gpu-miner/Cargo.toml` — optional `cudarc` dep + `cuda` feature.
- Tests under the crate.

## Risks / open questions

- `cudarc` ↔ driver/NVRTC version compat. Mitigation: pin a `cudarc` version; dynamic-load avoids a build-time toolkit dependency.
- Kernel-port fidelity: covered by the cross-backend parity test on a CUDA host.
- The `main.rs` compile-time→runtime refactor + the Metal/OpenCL shared-trait refactor are the structural changes; keep them minimal and behavior-preserving.

## Implementation order (Sub-project A only)

1. Shared `GpuBackend`/`DispatchOutcome` abstraction; refactor Metal + OpenCL onto it.
2. `--backend` flag + runtime selection (auto/explicit, with OpenCL fallback).
3. CUDA kernel port + `CudaMiner` (cudarc + NVRTC) behind the `cuda` feature.
4. Default-CI tests (no-CUDA build + packing/reference correctness).
5. One manual NVIDIA pool smoke (kernel parity + `share_accepted` vs SJ).

## Sequenced follow-on (separate spec)

Sub-project B — dinero-qt Solo/Pool UI integration: an `Sv2PoolMiner` controller that bundles + spawns these miners as a `QProcess`, parses the `--json` event stream, adds a Solo/Pool switch reusing the existing backend selector, payout = active wallet address, default pool = SJ (overridable). Out of scope for this spec.
