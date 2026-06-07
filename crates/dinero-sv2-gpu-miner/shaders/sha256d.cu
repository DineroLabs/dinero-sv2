// SHA-256d kernel for DineroCoin BlockHeader v1 (128 bytes), CUDA flavour.
// Ported from dinero-sv2/dinero-sv2-gpu-miner/shaders/sha256d.cu (the version
// after DineroLabs/dinero-sv2#1 fixed the LSW-first hash_meets_target).
//
// I/O contract:
//   header (32 u32, little-endian on the wire) — 128 raw header bytes
//                                                packed as u32::from_le_bytes
//   target (8  u32, big-endian words)          — target_words[0] is MSW,
//                                                u32::from_be_bytes(target[0..4])
//   nonce_start (u32)                          — each thread tries
//                                                nonce_start + tid
//   batch_size (u32)                           — number of valid nonce
//                                                offsets in this dispatch
//   result_nonces (u32[capacity])              — array of winning nonces
//   result_count (u32)                         — total winners detected
//                                                (may exceed capacity if the
//                                                target is unrealistically
//                                                loose; the host clamps
//                                                reads to capacity and logs)
//   capacity (u32)                             — size of result_nonces
//
// The result-array shape avoids the silent-drop-shares failure mode of a
// single-winner atomicCAS: every nonce that satisfies the share target is
// recorded, the host iterates the array, and any overflow (count > capacity)
// is surfaced as a log line so the operator can lower the batch size or
// raise the target.
//
// Embedded as a string by src/cuda_backend.rs for the NVRTC compile path.

extern "C" {

// SHA-256 constants in __constant__ memory: cached on chip and broadcast to
// all threads in a warp.
__constant__ unsigned int K[64] = {
    0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u, 0x3956c25bu, 0x59f111f1u, 0x923f82a4u, 0xab1c5ed5u,
    0xd807aa98u, 0x12835b01u, 0x243185beu, 0x550c7dc3u, 0x72be5d74u, 0x80deb1feu, 0x9bdc06a7u, 0xc19bf174u,
    0xe49b69c1u, 0xefbe4786u, 0x0fc19dc6u, 0x240ca1ccu, 0x2de92c6fu, 0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau,
    0x983e5152u, 0xa831c66du, 0xb00327c8u, 0xbf597fc7u, 0xc6e00bf3u, 0xd5a79147u, 0x06ca6351u, 0x14292967u,
    0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu, 0x53380d13u, 0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u,
    0xa2bfe8a1u, 0xa81a664bu, 0xc24b8b70u, 0xc76c51a3u, 0xd192e819u, 0xd6990624u, 0xf40e3585u, 0x106aa070u,
    0x19a4c116u, 0x1e376c08u, 0x2748774cu, 0x34b0bcb5u, 0x391c0cb3u, 0x4ed8aa4au, 0x5b9cca4fu, 0x682e6ff3u,
    0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u, 0x90befffau, 0xa4506cebu, 0xbef9a3f7u, 0xc67178f2u
};

__constant__ unsigned int H[8] = {
    0x6a09e667u, 0xbb67ae85u, 0x3c6ef372u, 0xa54ff53au,
    0x510e527fu, 0x9b05688cu, 0x1f83d9abu, 0x5be0cd19u
};

// __funnelshift_r((x,x),n) is one PRMT/SHF instruction on Volta+; the
// portable (x>>n)|(x<<(32-n)) idiom compiles to two SHFs and a LOP.
#define ROTR(x, n) __funnelshift_r((x), (x), (n))

#define CH(x, y, z)  (((x) & (y)) ^ (~(x) & (z)))
#define MAJ(x, y, z) (((x) & (y)) ^ ((x) & (z)) ^ ((y) & (z)))
#define EP0(x)       (ROTR((x),  2) ^ ROTR((x), 13) ^ ROTR((x), 22))
#define EP1(x)       (ROTR((x),  6) ^ ROTR((x), 11) ^ ROTR((x), 25))
#define SIG0(x)      (ROTR((x),  7) ^ ROTR((x), 18) ^ ((x) >> 3))
#define SIG1(x)      (ROTR((x), 17) ^ ROTR((x), 19) ^ ((x) >> 10))

// __byte_perm(x, 0, 0x0123) reverses the 4 bytes of x in a single PRMT.
__device__ __forceinline__ unsigned int swap_endian(unsigned int x) {
    return __byte_perm(x, 0u, 0x0123u);
}

__device__ __forceinline__ void sha256_transform(unsigned int* state, const unsigned int* block) {
    unsigned int W[64];
    unsigned int a, b, c, d, e, f, g, h_;
    unsigned int t1, t2;

    #pragma unroll
    for (int i = 0; i < 16; i++) {
        W[i] = block[i];
    }
    #pragma unroll
    for (int i = 16; i < 64; i++) {
        W[i] = SIG1(W[i - 2]) + W[i - 7] + SIG0(W[i - 15]) + W[i - 16];
    }

    a = state[0]; b = state[1]; c = state[2]; d = state[3];
    e = state[4]; f = state[5]; g = state[6]; h_ = state[7];

    #pragma unroll
    for (int i = 0; i < 64; i++) {
        t1 = h_ + EP1(e) + CH(e, f, g) + K[i] + W[i];
        t2 = EP0(a) + MAJ(a, b, c);
        h_ = g; g = f; f = e; e = d + t1;
        d = c; c = b; b = a; a = t1 + t2;
    }

    state[0] += a; state[1] += b; state[2] += c; state[3] += d;
    state[4] += e; state[5] += f; state[6] += g; state[7] += h_;
}

// Big-endian word-wise compare against target; returns 1 iff hash < target.
// Walk MSW → LSW: hash[0]/target[0] are the most significant 32 bits
// (state[0] is H0 from SHA-256, target_words[0] is u32_from_be_bytes of
// target_bytes[0..4]). Walking the array in reverse compares LSWs first
// and returns wrong results for any hash whose low word happens to fall
// below the target's low word.
__device__ __forceinline__ int hash_meets_target(const unsigned int* hash, const unsigned int* target) {
    #pragma unroll
    for (int i = 0; i < 8; i++) {
        if (hash[i] < target[i]) return 1;
        if (hash[i] > target[i]) return 0;
    }
    return 0;
}

// One thread = one nonce attempt. Each thread independently reconstructs
// the 128-byte header (with its own nonce), runs SHA-256 twice, compares
// against the target, and if the hash satisfies it appends its nonce to
// the shared result array via atomicAdd.
//
// The atomicAdd-into-array shape (instead of an atomicCAS-once flag)
// matters: at low share difficulty a 1M-nonce batch can produce many
// satisfying hashes, and the older single-winner shape silently dropped
// every winner past the first. Here every winner is captured up to
// `result_capacity` slots, and overflow (count > capacity) is reported
// to the host so it can either resize or warn.
__global__ void sha256d_mine(
    const unsigned int* __restrict__ header,        // 32 u32 = 128 bytes (LE on wire)
    const unsigned int* __restrict__ target,        // 8 u32 BE (MSW at index 0)
    unsigned int nonce_start,
    unsigned int batch_size,
    unsigned int* __restrict__ result_nonces,       // [out] u32[result_capacity]
    unsigned int* __restrict__ result_count,        // [out] total satisfying nonces
    unsigned int result_capacity
) {
    unsigned int tid   = blockIdx.x * blockDim.x + threadIdx.x;
    if (tid >= batch_size) return;

    unsigned int nonce = nonce_start + tid;

    // 128-byte header → two 64-byte SHA-256 input blocks.
    unsigned int block1[16];
    #pragma unroll
    for (int i = 0; i < 16; i++) {
        block1[i] = swap_endian(header[i]);
    }

    unsigned int block2[16];
    #pragma unroll
    for (int i = 0; i < 12; i++) {
        block2[i] = swap_endian(header[16 + i]);
    }
    block2[12] = swap_endian(nonce);
    block2[13] = swap_endian(header[29]);
    block2[14] = swap_endian(header[30]);
    block2[15] = swap_endian(header[31]);

    unsigned int state1[8];
    #pragma unroll
    for (int i = 0; i < 8; i++) state1[i] = H[i];
    sha256_transform(state1, block1);
    sha256_transform(state1, block2);

    // Padding block for a 128-byte (1024-bit) message.
    unsigned int pad_block[16];
    pad_block[0]  = 0x80000000u;
    #pragma unroll
    for (int i = 1; i < 14; i++) pad_block[i] = 0u;
    pad_block[14] = 0u;
    pad_block[15] = 1024u;
    sha256_transform(state1, pad_block);

    // Second SHA-256: hash the 32-byte first-hash output.
    unsigned int block3[16];
    #pragma unroll
    for (int i = 0; i < 8; i++) block3[i] = state1[i];
    block3[8]  = 0x80000000u;
    #pragma unroll
    for (int i = 9; i < 15; i++) block3[i] = 0u;
    block3[15] = 256u;

    unsigned int state2[8];
    #pragma unroll
    for (int i = 0; i < 8; i++) state2[i] = H[i];
    sha256_transform(state2, block3);

    if (hash_meets_target(state2, target)) {
        // atomicAdd both reserves a slot AND increments the total count;
        // the host then reads min(count, capacity) entries from the array
        // and detects overflow when count > capacity.
        unsigned int slot = atomicAdd(result_count, 1u);
        if (slot < result_capacity) {
            result_nonces[slot] = nonce;
        }
        // else: count keeps incrementing past capacity; nonce is dropped
        // here but the operator sees the overflow on the host side.
    }
}

} // extern "C"
