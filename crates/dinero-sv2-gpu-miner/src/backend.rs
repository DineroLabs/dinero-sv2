//! Shared GPU backend abstraction. Each backend (Metal/OpenCL/CUDA) hashes a
//! batch of nonces against a 256-bit target and reports the first match.
//!
//! The public `dispatch` interface takes the raw 32-byte big-endian target;
//! each backend packs it into 8 BE u32 words internally via `pack_target_be`.
use anyhow::Result;

#[derive(Debug, Clone, Copy)]
pub struct DispatchOutcome {
    pub found: bool,
    pub nonce: u32,
    pub elapsed_ms: f64,
}

/// One GPU hashing backend. `dispatch` searches `[nonce_start, nonce_start +
/// batch_size)` and returns the first nonce whose double-SHA256 ≤ target.
pub trait GpuBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn device_name(&self) -> &str;
    /// Max threads per workgroup/threadgroup — reported in the `gpu_ready` event.
    fn max_threads_per_group(&self) -> u64;
    fn dispatch(
        &self,
        header_bytes: &[u8; 128],
        target: &[u8; 32],
        nonce_start: u32,
        batch_size: u32,
    ) -> Result<DispatchOutcome>;
}

/// Pack a 32-byte target into 8 big-endian u32 words — the exact layout the
/// kernels' `hash_meets_target` walks (state[0]→state[7] MSW-first).
pub fn pack_target_be(target: &[u8; 32]) -> [u32; 8] {
    let mut words = [0u32; 8];
    for i in 0..8 {
        words[i] = u32::from_be_bytes([
            target[i * 4],
            target[i * 4 + 1],
            target[i * 4 + 2],
            target[i * 4 + 3],
        ]);
    }
    words
}

/// CPU double-SHA256 reference for an 80-byte header — the ground truth every
/// GPU backend must match. Test-only (used by the packing/parity unit tests).
#[cfg(test)]
pub fn sha256d_reference(header80: &[u8; 80]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let first = Sha256::digest(&header80[..]);
    let second = Sha256::digest(first);
    let mut out = [0u8; 32];
    out.copy_from_slice(&second);
    out
}

#[cfg(test)]
mod tests {
    use super::pack_target_be;

    #[test]
    fn sha256d_reference_known_vector() {
        // sha256d(80 zero bytes) — precomputed with Python hashlib.
        let got = super::sha256d_reference(&[0u8; 80]);
        assert_eq!(
            hex::encode(got),
            "4be7570e8f70eb093640c8468274ba759745a7aa2b7d25ab1e0421b259845014"
        );
    }

    #[test]
    fn packs_target_big_endian_words() {
        let mut t = [0u8; 32];
        t[0] = 0x00;
        t[1] = 0x00;
        t[2] = 0x00;
        t[3] = 0x7f; // word 0 = 0x0000007f
        t[28] = 0xde;
        t[29] = 0xad;
        t[30] = 0xbe;
        t[31] = 0xef; // word 7 = 0xdeadbeef
        let w = pack_target_be(&t);
        assert_eq!(w[0], 0x0000_007f);
        assert_eq!(w[7], 0xdead_beef);
    }
}
