use anyhow::{ensure, Result};
use bitcoin::hashes::{sha256d, Hash};
use num_bigint::BigUint;
use sha2::{compress256, digest::generic_array::GenericArray};

pub const IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];
pub const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

pub const GENESIS: &str = concat!(
    "010000000000000000000000000000000000000000000000000000000000000000000000",
    "3ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a",
    "29ab5f49ffff001d1dac2b7c"
);
pub const GENESIS_ID: &str = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";

pub fn decode<const N: usize>(s: &str) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    hex::decode_to_slice(s, &mut bytes)?;
    Ok(bytes)
}

pub fn sha256d_bytes(bytes: &[u8]) -> [u8; 32] {
    sha256d::Hash::hash(bytes).to_byte_array()
}

pub fn display_hash(mut digest: [u8; 32]) -> String {
    digest.reverse();
    hex::encode(digest)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target(pub [u8; 32]); // Big-endian integer.

impl Target {
    pub fn compact(bits: u32) -> Result<Self> {
        let size = bits >> 24;
        let mantissa = bits & 0x007fffff;
        ensure!(
            bits & 0x00800000 == 0 && mantissa != 0 && size <= 34,
            "Invalid compact target"
        );
        let value = if size <= 3 {
            BigUint::from(mantissa >> (8 * (3 - size)))
        } else {
            BigUint::from(mantissa) << (8 * (size - 3))
        };
        let bytes = value.to_bytes_be();
        ensure!(
            value.bits() > 0 && bytes.len() <= 32,
            "Compact target out of range"
        );
        let mut target = [0; 32];
        target[32 - bytes.len()..].copy_from_slice(&bytes);
        Ok(Self(target))
    }

    pub fn top(&self) -> u32 {
        u32::from_be_bytes(self.0[..4].try_into().unwrap())
    }

    #[inline]
    pub fn meets(&self, digest: &[u8; 32]) -> bool {
        digest.iter().rev().cmp(self.0.iter()).is_le()
    }
}

pub fn header(
    version: u32,
    previous: [u8; 32],
    merkle: [u8; 32],
    time: u32,
    bits: u32,
) -> [u8; 80] {
    let mut header = [0; 80];
    header[..4].copy_from_slice(&version.to_le_bytes());
    header[4..36].copy_from_slice(&previous);
    header[36..68].copy_from_slice(&merkle);
    header[68..72].copy_from_slice(&time.to_le_bytes());
    header[72..76].copy_from_slice(&bits.to_le_bytes());
    header
}

pub fn with_nonce(mut header: [u8; 80], nonce: u32) -> [u8; 80] {
    header[76..].copy_from_slice(&nonce.to_le_bytes());
    header
}

pub struct Midstate {
    pub words: [u32; 8],
    tail: GenericArray<u8, sha2::digest::consts::U64>,
}

impl Midstate {
    pub fn new(header: &[u8; 80]) -> Self {
        let mut words = IV;
        compress256(
            &mut words,
            std::slice::from_ref(GenericArray::from_slice(&header[..64])),
        );
        Self {
            words,
            tail: GenericArray::default(),
        }
        .with_tail(header)
    }

    pub fn with_tail(&self, header: &[u8; 80]) -> Self {
        let mut tail = GenericArray::default();
        tail[..12].copy_from_slice(&header[64..76]);
        tail[16] = 0x80;
        tail[62..].copy_from_slice(&640u16.to_be_bytes());
        Self {
            words: self.words,
            tail,
        }
    }

    pub fn tail_words(&self) -> [u32; 3] {
        std::array::from_fn(|i| u32::from_be_bytes(self.tail[i * 4..i * 4 + 4].try_into().unwrap()))
    }

    #[inline]
    pub fn hash(&self, nonce: u32) -> [u8; 32] {
        let mut first = self.words;
        let mut tail = self.tail;
        tail[12..16].copy_from_slice(&nonce.to_le_bytes());
        compress256(&mut first, &[tail]);
        let mut second = GenericArray::default();
        for (word, bytes) in first.iter().zip(second[..32].chunks_exact_mut(4)) {
            bytes.copy_from_slice(&word.to_be_bytes());
        }
        second[32] = 0x80;
        second[62..].copy_from_slice(&256u16.to_be_bytes());
        let mut words = IV;
        compress256(&mut words, &[second]);
        let mut digest = [0; 32];
        for (word, bytes) in words.iter().zip(digest.chunks_exact_mut(4)) {
            bytes.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }
}

pub fn arm_sha() -> bool {
    #[cfg(target_arch = "aarch64")]
    return std::arch::is_aarch64_feature_detected!("sha2");
    #[cfg(not(target_arch = "aarch64"))]
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_and_reference_agree() {
        let genesis = decode(GENESIS).unwrap();
        let nonce = u32::from_le_bytes(genesis[76..].try_into().unwrap());
        let digest = Midstate::new(&genesis).hash(nonce);
        assert_eq!(display_hash(digest), GENESIS_ID);
        assert!(Target::compact(0x1d00ffff).unwrap().meets(&digest));
        for header in [genesis, [0; 80], [255; 80]] {
            for nonce in [0, 1, 0x7c2bac1d, 0x10203040, u32::MAX] {
                assert_eq!(
                    Midstate::new(&header).hash(nonce),
                    sha256d_bytes(&with_nonce(header, nonce))
                );
            }
        }
    }

    #[test]
    fn compact_targets_and_order() {
        let target = Target::compact(0x1d00ffff).unwrap();
        assert_eq!(
            hex::encode(target.0),
            "00000000ffff0000000000000000000000000000000000000000000000000000"
        );
        let mut digest = target.0;
        digest.reverse();
        assert!(target.meets(&digest));
        digest[0] = 1;
        assert!(!target.meets(&digest));
        for bits in [0, 0x1d80ffff, 0x23000001, 0x22000100, 0x01000001] {
            assert!(Target::compact(bits).is_err(), "{bits:x}");
        }
        assert!(Target::compact(0x220000ff).is_ok());
        assert!(Target::compact(0x207fffff).is_ok());
    }
}
