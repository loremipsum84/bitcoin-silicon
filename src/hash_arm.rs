//! Interleaved ARMv8 SHA-256 scanner

use crate::hash::{Midstate, IV, K};
use core::arch::aarch64::*;

const LANES: usize = 4;

#[inline(always)]
unsafe fn rounds<const L: usize>(st: &mut [[uint32x4_t; 2]; L], msg: &mut [[uint32x4_t; 4]; L]) {
    let save = *st;
    let kp = K.as_ptr();
    for g in 0..16 {
        let k = vld1q_u32(kp.add(g * 4));
        for l in 0..L {
            let tmp = vaddq_u32(msg[l][g % 4], k);
            let abcd = st[l][0];
            st[l][0] = vsha256hq_u32(abcd, st[l][1], tmp);
            st[l][1] = vsha256h2q_u32(st[l][1], abcd, tmp);
            if g < 12 {
                msg[l][g % 4] = vsha256su1q_u32(
                    vsha256su0q_u32(msg[l][g % 4], msg[l][(g + 1) % 4]),
                    msg[l][(g + 2) % 4],
                    msg[l][(g + 3) % 4],
                );
            }
        }
    }
    for l in 0..L {
        st[l][0] = vaddq_u32(st[l][0], save[l][0]);
        st[l][1] = vaddq_u32(st[l][1], save[l][1]);
    }
}

#[inline(always)]
unsafe fn v(a: u32, b: u32, c: u32, d: u32) -> uint32x4_t {
    let arr = [a, b, c, d];
    vld1q_u32(arr.as_ptr())
}

// Lane loops index several parallel arrays; explicit indices keep the
// interleaving obvious to the reader and the optimizer.
#[allow(clippy::needless_range_loop)]
#[target_feature(enable = "sha2")]
pub unsafe fn scan(mid: &Midstate, top: u32, start: u32, count: u32, out: &mut Vec<u32>) {
    let tail = mid.tail_words();
    let mid = [
        vld1q_u32(mid.words.as_ptr()),
        vld1q_u32(mid.words.as_ptr().add(4)),
    ];
    let iv = [vld1q_u32(IV.as_ptr()), vld1q_u32(IV.as_ptr().add(4))];
    let m1 = v(0x8000_0000, 0, 0, 0);
    let m2 = vdupq_n_u32(0);
    let m3 = v(0, 0, 0, 640);
    let pad2a = v(0x8000_0000, 0, 0, 0);
    let pad2b = v(0, 0, 0, 256);
    let full = count as usize / LANES * LANES;
    let mut i = 0usize;
    while i < full {
        let mut st = [mid; LANES];
        let mut msg = [[m2; 4]; LANES];
        for l in 0..LANES {
            let nonce = start + (i + l) as u32;
            msg[l] = [v(tail[0], tail[1], tail[2], nonce.swap_bytes()), m1, m2, m3];
        }
        rounds::<LANES>(&mut st, &mut msg);
        let mut st2 = [iv; LANES];
        let mut msg2 = [[m2; 4]; LANES];
        for l in 0..LANES {
            msg2[l] = [st[l][0], st[l][1], pad2a, pad2b];
        }
        rounds::<LANES>(&mut st2, &mut msg2);
        for l in 0..LANES {
            let h7 = vgetq_lane_u32::<3>(st2[l][1]);
            if h7.swap_bytes() <= top {
                out.push(start + (i + l) as u32);
            }
        }
        i += LANES;
    }
    // Remainder (count not multiple of LANES)
    while i < count as usize {
        let nonce = start + i as u32;
        let mut st = [mid];
        let mut msg = [[v(tail[0], tail[1], tail[2], nonce.swap_bytes()), m1, m2, m3]];
        rounds::<1>(&mut st, &mut msg);
        let mut st2 = [iv];
        let mut msg2 = [[st[0][0], st[0][1], pad2a, pad2b]];
        rounds::<1>(&mut st2, &mut msg2);
        if vgetq_lane_u32::<3>(st2[0][1]).swap_bytes() <= top {
            out.push(nonce);
        }
        i += 1;
    }
}
