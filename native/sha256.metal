// Round unrolling and partial SHA-256d

#include <metal_stdlib>
using namespace metal;

struct Job {
    uint mid[8];
    uint pre[8];
    uint tail[3];
    uint w16;
    uint w17;
    uint target[8];
    uint start;
    uint count;
};

struct Hits {
    atomic_uint count;
    atomic_uint full;
    uint nonces[256];
};

constant uint K[64] = {
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
};

#define ROTR(x, n) rotate((x), (uint)(32 - (n)))
#define BS0(x) (ROTR(x, 2) ^ ROTR(x, 13) ^ ROTR(x, 22))
#define BS1(x) (ROTR(x, 6) ^ ROTR(x, 11) ^ ROTR(x, 25))
#define SS0(x) (ROTR(x, 7) ^ ROTR(x, 18) ^ ((x) >> 3))
#define SS1(x) (ROTR(x, 17) ^ ROTR(x, 19) ^ ((x) >> 10))
#define CH(e, f, g) ((g) ^ ((e) & ((f) ^ (g))))
#define MAJ(a, b, c) (((a) & (b)) | ((c) & ((a) | (b))))

#define ROUND(i)                                                     \
    {                                                                \
        uint t1 = h + BS1(e) + CH(e, f, g) + K[i] + w[(i) & 15];     \
        uint t2 = BS0(a) + MAJ(a, b, c);                             \
        h = g; g = f; f = e; e = d + t1;                             \
        d = c; c = b; b = a; a = t1 + t2;                            \
    }

#define SCHED(i)                                                     \
    w[(i) & 15] = SS1(w[((i) - 2) & 15]) + w[((i) - 7) & 15] +       \
                  SS0(w[((i) - 15) & 15]) + w[(i) & 15];

static inline uint bswap32(uint x) {
    return (x << 24) | ((x & 0xff00u) << 8) | ((x >> 8) & 0xff00u) | (x >> 24);
}

struct State {
    uint a, b, c, d, e, f, g, h;
};

inline State hash_through_round_60(constant Job &p, uint nonce, thread uint w[16]) {
    w[0] = p.tail[0]; w[1] = p.tail[1]; w[2] = p.tail[2];
    w[3] = bswap32(nonce);
    w[4] = 0x80000000u;
    w[5] = 0; w[6] = 0; w[7] = 0; w[8] = 0; w[9] = 0;
    w[10] = 0; w[11] = 0; w[12] = 0; w[13] = 0; w[14] = 0;
    w[15] = 640u;
    uint a = p.pre[0], b = p.pre[1], c = p.pre[2], d = p.pre[3];
    uint e = p.pre[4], f = p.pre[5], g = p.pre[6], h = p.pre[7];
    // Static schedule indices keep the rolling window in registers.
    ROUND(3) ROUND(4) ROUND(5) ROUND(6) ROUND(7) ROUND(8) ROUND(9) ROUND(10)
    ROUND(11) ROUND(12) ROUND(13) ROUND(14) ROUND(15)
    w[0] = p.w16; ROUND(16)
    w[1] = p.w17; ROUND(17)
    SCHED(18) ROUND(18) SCHED(19) ROUND(19) SCHED(20) ROUND(20) SCHED(21) ROUND(21)
    SCHED(22) ROUND(22) SCHED(23) ROUND(23) SCHED(24) ROUND(24) SCHED(25) ROUND(25)
    SCHED(26) ROUND(26) SCHED(27) ROUND(27) SCHED(28) ROUND(28) SCHED(29) ROUND(29)
    SCHED(30) ROUND(30) SCHED(31) ROUND(31) SCHED(32) ROUND(32) SCHED(33) ROUND(33)
    SCHED(34) ROUND(34) SCHED(35) ROUND(35) SCHED(36) ROUND(36) SCHED(37) ROUND(37)
    SCHED(38) ROUND(38) SCHED(39) ROUND(39) SCHED(40) ROUND(40) SCHED(41) ROUND(41)
    SCHED(42) ROUND(42) SCHED(43) ROUND(43) SCHED(44) ROUND(44) SCHED(45) ROUND(45)
    SCHED(46) ROUND(46) SCHED(47) ROUND(47) SCHED(48) ROUND(48) SCHED(49) ROUND(49)
    SCHED(50) ROUND(50) SCHED(51) ROUND(51) SCHED(52) ROUND(52) SCHED(53) ROUND(53)
    SCHED(54) ROUND(54) SCHED(55) ROUND(55) SCHED(56) ROUND(56) SCHED(57) ROUND(57)
    SCHED(58) ROUND(58) SCHED(59) ROUND(59) SCHED(60) ROUND(60) SCHED(61) ROUND(61)
    SCHED(62) ROUND(62) SCHED(63) ROUND(63)
    uint h0 = p.mid[0] + a, h1 = p.mid[1] + b, h2 = p.mid[2] + c, h3 = p.mid[3] + d;
    uint h4 = p.mid[4] + e, h5 = p.mid[5] + f, h6 = p.mid[6] + g, h7 = p.mid[7] + h;

    w[0] = h0; w[1] = h1; w[2] = h2; w[3] = h3;
    w[4] = h4; w[5] = h5; w[6] = h6; w[7] = h7;
    w[8] = 0x80000000u;
    w[9] = 0; w[10] = 0; w[11] = 0; w[12] = 0; w[13] = 0; w[14] = 0;
    w[15] = 256u;
    a = 0x6a09e667u; b = 0xbb67ae85u; c = 0x3c6ef372u; d = 0xa54ff53au;
    e = 0x510e527fu; f = 0x9b05688cu; g = 0x1f83d9abu; h = 0x5be0cd19u;
    ROUND(0) ROUND(1) ROUND(2) ROUND(3) ROUND(4) ROUND(5) ROUND(6) ROUND(7)
    ROUND(8) ROUND(9) ROUND(10) ROUND(11) ROUND(12) ROUND(13) ROUND(14) ROUND(15)
    SCHED(16) ROUND(16) SCHED(17) ROUND(17) SCHED(18) ROUND(18) SCHED(19) ROUND(19)
    SCHED(20) ROUND(20) SCHED(21) ROUND(21) SCHED(22) ROUND(22) SCHED(23) ROUND(23)
    SCHED(24) ROUND(24) SCHED(25) ROUND(25) SCHED(26) ROUND(26) SCHED(27) ROUND(27)
    SCHED(28) ROUND(28) SCHED(29) ROUND(29) SCHED(30) ROUND(30) SCHED(31) ROUND(31)
    SCHED(32) ROUND(32) SCHED(33) ROUND(33) SCHED(34) ROUND(34) SCHED(35) ROUND(35)
    SCHED(36) ROUND(36) SCHED(37) ROUND(37) SCHED(38) ROUND(38) SCHED(39) ROUND(39)
    SCHED(40) ROUND(40) SCHED(41) ROUND(41) SCHED(42) ROUND(42) SCHED(43) ROUND(43)
    SCHED(44) ROUND(44) SCHED(45) ROUND(45) SCHED(46) ROUND(46) SCHED(47) ROUND(47)
    SCHED(48) ROUND(48) SCHED(49) ROUND(49) SCHED(50) ROUND(50) SCHED(51) ROUND(51)
    SCHED(52) ROUND(52) SCHED(53) ROUND(53) SCHED(54) ROUND(54) SCHED(55) ROUND(55)
    SCHED(56) ROUND(56) SCHED(57) ROUND(57) SCHED(58) ROUND(58) SCHED(59) ROUND(59)
    SCHED(60) ROUND(60)
    return {a, b, c, d, e, f, g, h};
}

inline State finish(State s, thread uint w[16]) {
    uint a=s.a, b=s.b, c=s.c, d=s.d, e=s.e, f=s.f, g=s.g, h=s.h;
    SCHED(61) ROUND(61) SCHED(62) ROUND(62) SCHED(63) ROUND(63)
    return {a + 0x6a09e667u, b + 0xbb67ae85u, c + 0x3c6ef372u, d + 0xa54ff53au,
            e + 0x510e527fu, f + 0x9b05688cu, g + 0x1f83d9abu, h + 0x5be0cd19u};
}

kernel void search(constant Job &job [[buffer(0)]], device Hits &hits [[buffer(1)]],
                   uint index [[thread_position_in_grid]]) {
    if (index >= job.count) return;
    uint nonce = job.start + index;
    uint w[16];
    State state = hash_through_round_60(job, nonce, w);
    // e after round 60 shifts to h by round 63. Bitcoin compares the
    // digest as a little-endian integer, so swapped H7 is the top word.
    uint top = bswap32(state.e + 0x5be0cd19u);
    if (top > job.target[0]) return;
    if (top == job.target[0]) {
        State digest = finish(state, w);
        atomic_fetch_add_explicit(&hits.full, 1u, memory_order_relaxed);
        uint remaining[7] = {digest.g, digest.f, digest.e, digest.d, digest.c, digest.b, digest.a};
        #pragma clang loop unroll(full)
        for (uint i = 0; i < 7; ++i) {
            uint word = bswap32(remaining[i]);
            if (word > job.target[i + 1]) return;
            if (word < job.target[i + 1]) break;
        }
    }
    uint slot = atomic_fetch_add_explicit(&hits.count, 1u, memory_order_relaxed);
    if (slot < 256) hits.nonces[slot] = nonce;
}

kernel void vector_digest(constant Job &job [[buffer(0)]], device uint *out [[buffer(1)]],
                          uint index [[thread_position_in_grid]]) {
    if (index >= job.count) return;
    uint w[16];
    State partial = hash_through_round_60(job, job.start + index, w);
    State digest = finish(partial, w);
    out[0] = digest.a;
    out[1] = digest.b;
    out[2] = digest.c;
    out[3] = digest.d;
    out[4] = digest.e;
    out[5] = digest.f;
    out[6] = digest.g;
    out[7] = digest.h;
}
