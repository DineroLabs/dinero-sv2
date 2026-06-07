# SV2 GPU Miner — Native CUDA Backend Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a native CUDA backend to `dinero-sv2-gpu-miner` so SV2 GPU pool mining reaches Metal/OpenCL/CUDA parity with the dinero-qt solo miner.

**Architecture:** Introduce one shared `GpuBackend` trait (uniform `DispatchOutcome`), refactor the existing Metal + OpenCL miners onto it, add runtime `--backend auto|metal|opencl|cuda` selection (replacing the compile-time `type GpuMiner` alias), then add `CudaMiner` (cudarc + NVRTC) behind an optional `cuda` Cargo feature, porting the proven dinero-v8 `sha256d_cu` kernel. Default builds/tests require no CUDA; CUDA correctness is proven by a manual GPU smoke.

**Tech Stack:** Rust, `cudarc` (CUDA Driver API + NVRTC, dynamic-load), `ocl` (existing OpenCL), `metal` (existing), `clap`, `anyhow`.

**Spec:** `docs/superpowers/specs/2026-06-06-cuda-backend-design.md`

---

## Status (2026-06-06)

**Landed inline (branch `feat/sv2-gpu-cuda-backend`), committed; macOS-side verified:**
- **Task 1** (`499f60b`) — shared `GpuBackend`/`DispatchOutcome` + `pack_target_be`; Metal/OpenCL refactored onto it; packing + sha256d-reference unit tests gate (packer neuter-checked).
- **Task 2** (`e8c535d`) — runtime `--backend auto|metal|opencl|cuda`; pure `choose_backend` (6 unit tests). Device-verified: Metal inits the M4 Max; `cuda`/`opencl` give actionable errors on macOS.
- **Task 3** (`4da4e14`) — `shaders/sha256d.cu` carried over **verbatim** from the tested dinero-v8 kernel (result-array contract); empty `cuda` Cargo feature + stubbed `cuda_backend.rs` (kernel embedded via `include_str!`, honest error path). Default **and** `--features cuda` builds/tests green on macOS; workspace default build is CUDA-free.

**Unverified here — pre-merge gate, independent of the CUDA work:** the OpenCL/non-macOS code is `cfg(not(target_os = "macos"))`, so this Mac never compiled it. Task 1's OpenCL refactor (removed local `DispatchOutcome`, `pack_target_be` swap, removed inherent accessors, `impl GpuBackend`) and Task 2's non-macOS `build_backend` arm — i.e. the **Linux default build (OpenCL, no `cuda` feature)** — were written but compiled only by inspection. The pattern is symmetric to the verified Metal path (same shared trait, same inherent-priority `self.dispatch()` delegation, which the Metal compile proves), but **before merge: run `cargo build` + `cargo test` (default features) on Linux** to gate the OpenCL path. This is required regardless of, and separate from, the CUDA/NVIDIA host work below.

**`--json` schema note:** Task 2 renamed the `startup` event's `backend` field to `backend_requested` (the resolved backend is still reported in `gpu_ready.backend`). Any current `--json` consumer — and Sub-project B's QProcess parser — should expect that.

**Deferred to a Linux/NVIDIA host (CUDA cannot compile or run on Apple Silicon):**
- **Task 4 (real body)** — replace the `cuda_backend.rs` stub with the cudarc Driver-API + NVRTC implementation; add `dep:cudarc` to the `cuda` feature; wire the result-array buffers (`result_nonces[capacity]` / `result_count` / `result_capacity` + `batch_size`) to the kernel's contract; the non-macOS CUDA arm of `build_backend` is already scaffolded.
- **Task 5** — kernel parity + real pool smoke.

> **Caveat for Task 5 (found during Task 2):** this crate is **bin-only** (no `lib` target), so a `tests/` integration test cannot reach internal items like `backend::sha256d_reference`. The parity test must live as an in-crate `#[cfg(test)]` unit test (e.g. in `cuda_backend.rs`, gated `cfg(all(test, feature = "cuda"))`), **not** as `tests/cuda_parity.rs`. `sha256d_reference` is already `#[cfg(test)]`.

> **Kernel I/O note:** the carried-over CUDA kernel uses the result-**array** shape (`result_nonces`/`result_count`/`result_capacity`, with a `batch_size` arg), a deliberate improvement over the OpenCL/Metal single-winner (`result_nonce`/`result_found`) shape — it avoids dropping multiple winners in a low-difficulty batch. Task 4's `CudaMiner::dispatch` therefore does **not** byte-mirror `OpenClMiner`'s buffers; it allocates the array buffers and returns the lowest winning nonce to satisfy the shared `DispatchOutcome`.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/dinero-sv2-gpu-miner/src/backend.rs` (new) | Shared `DispatchOutcome` + `GpuBackend` trait + `pack_target_be()` helper + CPU `sha256d` reference (test support) |
| `crates/dinero-sv2-gpu-miner/src/metal_backend.rs` (modify) | Refactor onto shared `DispatchOutcome`; `impl GpuBackend` |
| `crates/dinero-sv2-gpu-miner/src/opencl_backend.rs` (modify) | Same refactor; reuse `pack_target_be()` |
| `crates/dinero-sv2-gpu-miner/src/cuda_backend.rs` (new, `cfg(feature="cuda")`) | `CudaMiner`: cudarc context + NVRTC-compiled kernel + `dispatch()`; `impl GpuBackend` |
| `crates/dinero-sv2-gpu-miner/shaders/sha256d.cu` (new) | CUDA-C kernel ported from `dinero-v8/miner/cmake/sha256d_cu_src.cpp.in` |
| `crates/dinero-sv2-gpu-miner/src/main.rs` (modify) | `--backend` arg; runtime backend selection; `start_hashing_gpu` takes `Arc<dyn GpuBackend>` |
| `crates/dinero-sv2-gpu-miner/Cargo.toml` (modify) | Optional `cudarc` dep + `cuda` feature |

---

## Task 1: Shared `GpuBackend` abstraction + `pack_target_be` (default-CI testable)

**Files:**
- Create: `crates/dinero-sv2-gpu-miner/src/backend.rs`
- Modify: `crates/dinero-sv2-gpu-miner/src/metal_backend.rs`, `src/opencl_backend.rs`, `src/main.rs` (add `mod backend;`)

- [ ] **Step 1: Write the failing test** for the BE target-packing helper (this is the cross-backend contract; OpenCL already inlines this logic at `opencl_backend.rs` dispatch — we extract + test it).

In `src/backend.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::pack_target_be;

    #[test]
    fn packs_target_big_endian_words() {
        let mut t = [0u8; 32];
        t[0] = 0x00; t[1] = 0x00; t[2] = 0x00; t[3] = 0x7f; // word 0 = 0x0000007f
        t[28] = 0xde; t[29] = 0xad; t[30] = 0xbe; t[31] = 0xef; // word 7 = 0xdeadbeef
        let w = pack_target_be(&t);
        assert_eq!(w[0], 0x0000_007f);
        assert_eq!(w[7], 0xdead_beef);
    }
}
```

- [ ] **Step 2: Run it, verify it fails** (`pack_target_be` undefined)

Run: `cargo test -p dinero-sv2-gpu-miner pack_target --no-default-features`
Expected: FAIL — `cannot find function pack_target_be`.

- [ ] **Step 3: Implement `backend.rs`** — shared outcome, trait, helper.

```rust
//! Shared GPU backend abstraction. Each backend (Metal/OpenCL/CUDA) hashes a
//! batch of nonces against a 256-bit target and reports the first match.
use anyhow::Result;

#[derive(Debug, Clone, Copy)]
pub struct DispatchOutcome {
    pub found: bool,
    pub nonce: u32,
    pub elapsed_ms: f64,
}

/// One GPU hashing backend. `dispatch` takes the raw 80→128-byte header and a
/// raw 32-byte big-endian target; each backend packs the target into 8 BE u32
/// words internally (see `pack_target_be`).
pub trait GpuBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn device_name(&self) -> &str;
    fn dispatch(
        &self,
        header_bytes: &[u8; 128],
        target: &[u8; 32],
        nonce_start: u32,
        batch_size: u32,
    ) -> Result<DispatchOutcome>;
}

/// Pack a 32-byte target into 8 big-endian u32 words. This is the exact layout
/// the kernels' `hash_meets_target` walks (state[7]→state[0] BE comparison).
pub fn pack_target_be(target: &[u8; 32]) -> [u32; 8] {
    let mut words = [0u32; 8];
    for i in 0..8 {
        words[i] = u32::from_be_bytes([
            target[i * 4], target[i * 4 + 1], target[i * 4 + 2], target[i * 4 + 3],
        ]);
    }
    words
}

/// CPU double-SHA256 reference for an 80-byte header — the ground truth the
/// GPU kernels (all backends) must match. Used by the Task 5 parity test.
pub fn sha256d_reference(header80: &[u8; 80]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let first = Sha256::digest(&header80[..]);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}
```
(`sha2` is already a workspace dependency via `dinero-sv2-common`; add `sha2 = "0.10"` to this crate's `[dependencies]` if `cargo build` reports it missing.)
Add `mod backend;` and `pub use backend::{DispatchOutcome, GpuBackend, pack_target_be};` near the top of `src/main.rs` (where the other `mod`s are declared, ~line 48).

- [ ] **Step 4: Run the test, verify PASS**

Run: `cargo test -p dinero-sv2-gpu-miner pack_target --no-default-features`
Expected: PASS.

- [ ] **Step 5: Refactor Metal + OpenCL onto the shared types.**

In `src/opencl_backend.rs`: delete the local `pub struct DispatchOutcome { … }` (lines ~51-55) and `use crate::backend::{DispatchOutcome, GpuBackend, pack_target_be};`. Replace the inline target-packing loop in `dispatch` with `let target_words = pack_target_be(target);`. Add at the end of the file:
```rust
impl GpuBackend for OpenClMiner {
    fn name(&self) -> &'static str { "opencl" }
    fn device_name(&self) -> &str { &self.inner.device_name }
    fn dispatch(&self, h: &[u8;128], t: &[u8;32], n: u32, b: u32) -> anyhow::Result<DispatchOutcome> {
        OpenClMiner::dispatch(self, h, t, n, b)
    }
}
```
Do the equivalent in `src/metal_backend.rs` (remove its local `DispatchOutcome`, `use crate::backend::*`, add `impl GpuBackend for MetalMiner` delegating to its inherent `dispatch`, `name() -> "metal"`). Keep each backend's inherent `dispatch`/`init` as-is otherwise.

- [ ] **Step 6: Verify the whole crate still builds + tests on the host (no GPU work runs at compile)**

Run: `cargo build -p dinero-sv2-gpu-miner` then `cargo test -p dinero-sv2-gpu-miner --no-default-features`
Expected: builds; `pack_target` test passes. (On macOS the Metal path compiles; on Linux the OpenCL path compiles.)

- [ ] **Step 7: Commit**
```bash
git add crates/dinero-sv2-gpu-miner/src/backend.rs crates/dinero-sv2-gpu-miner/src/metal_backend.rs crates/dinero-sv2-gpu-miner/src/opencl_backend.rs crates/dinero-sv2-gpu-miner/src/main.rs
git commit -m "refactor(gpu-miner): shared GpuBackend trait + pack_target_be"
```

---

## Task 2: `--backend` flag + runtime selection (default-CI testable)

**Files:**
- Modify: `crates/dinero-sv2-gpu-miner/src/main.rs` (Args ~line 70; `GpuMiner` alias ~line 58-66; `start_hashing_gpu` signature ~line 558)

- [ ] **Step 1: Write the failing test** for the selection decision (pure function, no GPU).

In `src/main.rs` (a `#[cfg(test)] mod backend_select_tests`):
```rust
#[cfg(test)]
mod backend_select_tests {
    use super::{choose_backend, BackendChoice};
    // choose_backend returns the NAME it would init, given the requested
    // choice and availability flags (so it's testable without a GPU).
    #[test]
    fn auto_prefers_cuda_then_opencl_on_non_macos() {
        assert_eq!(choose_backend(BackendChoice::Auto, false, true, true), Ok("cuda"));
        assert_eq!(choose_backend(BackendChoice::Auto, false, false, true), Ok("opencl"));
    }
    #[test]
    fn auto_uses_metal_on_macos() {
        assert_eq!(choose_backend(BackendChoice::Auto, true, false, false), Ok("metal"));
    }
    #[test]
    fn explicit_cuda_without_cuda_errors() {
        assert!(choose_backend(BackendChoice::Cuda, false, false, true).is_err());
    }
}
```

- [ ] **Step 2: Run it, verify it fails**

Run: `cargo test -p dinero-sv2-gpu-miner backend_select --no-default-features`
Expected: FAIL — `choose_backend` / `BackendChoice` undefined.

- [ ] **Step 3: Implement the choice enum + pure selector + clap arg.**

In `src/main.rs` add:
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum BackendChoice { Auto, Metal, Opencl, Cuda }

/// Pure backend decision (no GPU init). `is_macos`, `cuda_available`,
/// `opencl_available` are passed so this is unit-testable.
fn choose_backend(
    choice: BackendChoice, is_macos: bool, cuda_available: bool, opencl_available: bool,
) -> Result<&'static str, String> {
    match choice {
        BackendChoice::Metal if is_macos => Ok("metal"),
        BackendChoice::Metal => Err("metal is macOS-only".into()),
        BackendChoice::Cuda if cuda_available => Ok("cuda"),
        BackendChoice::Cuda => Err("CUDA backend requested but no NVIDIA driver/NVRTC found — install the CUDA runtime, or use --backend opencl / the CPU miner".into()),
        BackendChoice::Opencl if opencl_available => Ok("opencl"),
        BackendChoice::Opencl => Err("OpenCL requested but no OpenCL platform/GPU found".into()),
        BackendChoice::Auto if is_macos => Ok("metal"),
        BackendChoice::Auto if cuda_available => Ok("cuda"),
        BackendChoice::Auto if opencl_available => Ok("opencl"),
        BackendChoice::Auto => Err("no usable GPU backend (no CUDA/OpenCL) — use the CPU miner".into()),
    }
}
```
Add to `struct Args` (after `payout_script_hex`, ~line 78):
```rust
    /// GPU backend: auto (default), metal, opencl, cuda
    #[arg(long, value_enum, default_value_t = BackendChoice::Auto)]
    backend: BackendChoice,
```

- [ ] **Step 4: Replace the compile-time alias with runtime construction.**

Delete the `#[cfg(target_os=…)] type GpuMiner = …` / `BACKEND_NAME` block (~lines 58-66). Add a constructor that returns the boxed backend, probing availability:
```rust
fn build_backend(choice: BackendChoice) -> Result<std::sync::Arc<dyn backend::GpuBackend>> {
    let is_macos = cfg!(target_os = "macos");
    // Availability is determined by trying init(); cheap and authoritative.
    #[cfg(target_os = "macos")]
    let metal = metal_backend::MetalMiner::init().ok();
    #[cfg(not(target_os = "macos"))]
    let opencl = opencl_backend::OpenClMiner::init().ok();
    #[cfg(feature = "cuda")]
    let cuda = cuda_backend::CudaMiner::init().ok();
    #[cfg(not(feature = "cuda"))]
    let cuda: Option<()> = None;

    let cuda_avail = cfg!(feature = "cuda") && {
        #[cfg(feature = "cuda")] { cuda.is_some() }
        #[cfg(not(feature = "cuda"))] { false }
    };
    #[cfg(target_os = "macos")] let opencl_avail = false;
    #[cfg(not(target_os = "macos"))] let opencl_avail = opencl.is_some();

    match choose_backend(choice, is_macos, cuda_avail, opencl_avail).map_err(|e| anyhow::anyhow!(e))? {
        #[cfg(target_os = "macos")] "metal" => Ok(std::sync::Arc::new(metal.unwrap())),
        #[cfg(all(not(target_os = "macos"), feature = "cuda"))] "cuda" => Ok(std::sync::Arc::new(cuda.unwrap())),
        #[cfg(not(target_os = "macos"))] "opencl" => Ok(std::sync::Arc::new(opencl.unwrap())),
        other => anyhow::bail!("backend {other} not compiled in this build"),
    }
}
```
Change `start_hashing_gpu`'s `gpu: GpuMiner` parameter (~line 558) to `gpu: std::sync::Arc<dyn backend::GpuBackend>`, and update its internal `gpu.dispatch(...)` calls (already match the trait signature) and any `BACKEND_NAME` use → `gpu.name()`. Update the call site that constructed `GpuMiner` to call `build_backend(args.backend)?` once and clone the `Arc` into each `start_hashing_gpu`.

- [ ] **Step 5: Run tests + build**

Run: `cargo test -p dinero-sv2-gpu-miner backend_select --no-default-features` then `cargo build -p dinero-sv2-gpu-miner`
Expected: selection tests PASS; crate builds (CUDA arm compiled out without the feature).

- [ ] **Step 6: Commit**
```bash
git add crates/dinero-sv2-gpu-miner/src/main.rs
git commit -m "feat(gpu-miner): runtime --backend selection (auto|metal|opencl|cuda)"
```

---

## Task 3: Port the CUDA kernel

**Files:**
- Create: `crates/dinero-sv2-gpu-miner/shaders/sha256d.cu`
- Source of truth: `/Users/haydarevich/src/dinero-v8/miner/cmake/sha256d_cu_src.cpp.in` (the embedded CUDA-C kernel string) and `dinero-v8/miner/src/gpu/cuda_backend.cpp` (host launch contract)

- [ ] **Step 1: Copy the kernel.** Extract the CUDA-C kernel body from `sha256d_cu_src.cpp.in` (it's a string literal — take the C between the delimiters) into `shaders/sha256d.cu`. Keep the entry-point name and parameter order; the kernel must take `(const unsigned char* header /*128B*/, const unsigned int* target_be /*8*/, unsigned int nonce_start, unsigned int* result_nonce, unsigned int* result_found)` and, for each thread `i`, set `header[76..80] = nonce_start + i` (LE), compute double-SHA256, and on `hash_be ≤ target_be` do an atomic write of the nonce into `result_nonce` + set `result_found = 1`. If the dinero-qt entry differs, adapt the OpenCL `sha256d.cl` parameter contract (they already match across Metal/OpenCL) so all three backends share one ABI.

- [ ] **Step 2: Record the chosen entry-point name** as a `const KERNEL_ENTRY: &str` you'll reference from `cuda_backend.rs` (Task 4). No build/test here — kernel correctness is proven by the GPU parity test in Task 5; default CI only checks it's present/embeddable.

- [ ] **Step 3: Commit**
```bash
git add crates/dinero-sv2-gpu-miner/shaders/sha256d.cu
git commit -m "feat(gpu-miner): add CUDA sha256d kernel (port of dinero-v8 sha256d_cu)"
```

---

## Task 4: `CudaMiner` backend (cudarc) behind the `cuda` feature

**Files:**
- Modify: `crates/dinero-sv2-gpu-miner/Cargo.toml`
- Create: `crates/dinero-sv2-gpu-miner/src/cuda_backend.rs`
- Modify: `crates/dinero-sv2-gpu-miner/src/main.rs` (`#[cfg(feature="cuda")] mod cuda_backend;`)

- [ ] **Step 1: Add the optional dep + feature** in `Cargo.toml`:
```toml
[features]
default = []
cuda = ["dep:cudarc"]

[dependencies]
cudarc = { version = "0.12", features = ["nvrtc", "driver", "dynamic-loading"], optional = true }
```
(Pin to a current `cudarc`; `dynamic-loading` means no CUDA toolkit at build time.)

- [ ] **Step 2: Write `cuda_backend.rs`** mirroring `opencl_backend.rs` 1:1 in shape — `CudaMiner { inner: Arc<Inner> }`, `Inner { ctx, module, func, device_name, buffers: Mutex<DispatchBuffers> }`. `init()`: `cudarc::driver::CudaDevice::new(0)` (clean error → "no CUDA device/driver…"), compile `include_str!("../shaders/sha256d.cu")` via `cudarc::nvrtc::compile_ptx`, load the module + function `KERNEL_ENTRY`, allocate the reused device buffers (header 128B, target 8×u32, result_nonce 1×u32, result_found 1×u32). `dispatch(h, t, n, b)`: `let target_words = pack_target_be(t);` copy header+target up, zero results, `launch` with grid/block sized to `b` (block 256, grid `b.div_ceil(256)`), copy results back, return `DispatchOutcome`. Add `impl GpuBackend for CudaMiner { name()->"cuda", device_name(), dispatch() }`. Use the `opencl_backend.rs` `dispatch` body (already read) as the structural template — same buffer lifecycle, same `pack_target_be`, same outcome.

- [ ] **Step 3: Wire into main.rs** — `#[cfg(feature="cuda")] mod cuda_backend;` near the other mods.

- [ ] **Step 4: Prove the no-CUDA contract holds (default CI):**

Run: `cargo build -p dinero-sv2-gpu-miner` (no feature) → builds, no cudarc.
Run: `cargo test -p dinero-sv2-gpu-miner --no-default-features` → Task 1/2 tests pass.
Run (on a host WITHOUT a CUDA driver): `cargo build -p dinero-sv2-gpu-miner --features cuda` → compiles (dynamic-load; driver only needed at runtime).
Expected: all three succeed.

- [ ] **Step 5: Commit**
```bash
git add crates/dinero-sv2-gpu-miner/Cargo.toml crates/dinero-sv2-gpu-miner/src/cuda_backend.rs crates/dinero-sv2-gpu-miner/src/main.rs
git commit -m "feat(gpu-miner): native CUDA backend (cudarc + NVRTC) behind cuda feature"
```

---

## Task 5: Manual NVIDIA GPU smoke (kernel parity + real pool) — NOT default CI

**Runs only on a host with an NVIDIA GPU + CUDA driver.** Document this in `crates/dinero-sv2-gpu-miner/README.md` under "CUDA backend — manual verification".

- [ ] **Step 1: Build with CUDA on the NVIDIA box**

Run: `cargo build --release -p dinero-sv2-gpu-miner --features cuda`
Expected: builds; binary present.

- [ ] **Step 2: Kernel parity check.** Run a `--features cuda`-gated integration test (`tests/cuda_parity.rs`, `#[ignore]` by default) that, for a fixed header+target, asserts `CudaMiner::dispatch` returns the **same** first-valid nonce as `OpenClMiner::dispatch` and as a CPU `sha256d` reference (`backend::sha256d_reference`).

Run: `cargo test --release -p dinero-sv2-gpu-miner --features cuda -- --ignored cuda_parity`
Expected: PASS (CUDA nonce == OpenCL nonce == CPU reference).

- [ ] **Step 3: Real pool smoke** against the live SJ pool:

Run:
```bash
./target/release/dinero-sv2-gpu-miner --backend cuda \
  --pool 173.249.200.59:4444 \
  --server-pubkey bcaa90dba639e2d57baa4c6de8c88647a82f02669cb0395f0d9a44c0e4ec2931 \
  --payout-script-hex 5120<32-byte-program> --json
```
Expected (within ~30s): `connected` → `channel_open` → `new_job` → `hashrate` (GPU MH/s) → `share_submitted` → `share_accepted` (mirrors the CPU E2E done 2026-06-06).

- [ ] **Step 4: Commit the README + test**
```bash
git add crates/dinero-sv2-gpu-miner/README.md crates/dinero-sv2-gpu-miner/tests/cuda_parity.rs crates/dinero-sv2-gpu-miner/src/backend.rs
git commit -m "test(gpu-miner): CUDA parity + manual pool smoke; document GPU verification"
```

---

## Done when
- Default `cargo test --workspace` passes with **no CUDA toolkit/driver** (Tasks 1–4).
- `--features cuda` builds on a no-driver host (Task 4 Step 4).
- On an NVIDIA box: kernel parity passes and `--backend cuda` gets `share_accepted` from the SJ pool (Task 5).
- Metal/OpenCL behavior is unchanged (existing paths only refactored onto the shared trait).
