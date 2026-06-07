# dinero-sv2-gpu-miner

GPU-backed Stratum V2 pool miner for Dinero. Single binary, runtime
`--backend auto|metal|opencl|cuda` selection.

| Backend | Platforms | Build flag | Status |
|---|---|---|---|
| Metal  | macOS                  | (default, target-gated)  | tested |
| OpenCL | Linux / Windows (AMD / NVIDIA / Intel / Mesa) | (default, target-gated) | tested |
| CUDA   | Linux / Windows on NVIDIA | `--features cuda`     | tested (RTX 4060, CUDA 12.2) |

The CUDA backend uses `cudarc 0.13` with the `dynamic-linking` variant — no
CUDA toolkit is required at build time; the NVIDIA driver + NVRTC only need
to be present when `--backend cuda` actually launches.

## Build

```sh
# Default (Metal on macOS, OpenCL on Linux/Windows)
cargo build --release -p dinero-sv2-gpu-miner

# Plus the optional CUDA backend (Linux/Windows + NVIDIA only)
cargo build --release -p dinero-sv2-gpu-miner --features cuda
```

Windows note: `cl-sys` (the OpenCL SDK shim) searches `OCL_SDK_Light/lib/x86_64`
by default. The NVIDIA CUDA Toolkit ships its own `OpenCL.lib` — either install
the OCL SDK Light, or add a workspace-local `.cargo/config.toml`:

```toml
[target.x86_64-pc-windows-msvc]
rustflags = ["-L", "C:/Program Files/NVIDIA GPU Computing Toolkit/CUDA/v12.2/lib/x64"]
```

## CUDA backend — manual verification

The CUDA kernel parity test and live pool smoke require an NVIDIA GPU + CUDA
driver. They run only when explicitly invoked (default CI skips them).

### Kernel parity (in-crate, `#[ignore]` by default)

```sh
cargo test --release -p dinero-sv2-gpu-miner --features cuda \
  -- --ignored cuda_parity
```

Asserts that `CudaMiner::dispatch` returns the **lowest** winning nonce for a
fixed header + loose target, and that this nonce's CPU `sha256d` (computed
inline in the test) actually beats the target. Catches kernel false-positives
and the kernel's MSW→LSW BE compare direction.

### Live pool smoke

Replace the payout script with one of your own taproot/quantum-safe outputs:

```sh
./target/release/dinero-sv2-gpu-miner \
  --backend cuda \
  --pool 173.249.200.59:4444 \
  --server-pubkey bcaa90dba639e2d57baa4c6de8c88647a82f02669cb0395f0d9a44c0e4ec2931 \
  --payout-script-hex 5120<32-byte-program> \
  --json
```

Expected within ~30 s: `startup` → `gpu_ready` → `connected` → `channel_open`
→ `new_job` → `hashrate` → `share_submitted` → `share_accepted`. Reference run
on RTX 4060 Laptop GPU (CUDA 12.2): sustained 757 MH/s, share accepted on
sequence 1.
