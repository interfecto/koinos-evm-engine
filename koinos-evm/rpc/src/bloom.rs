//! Ethereum 2048-bit log bloom (yellow-paper M3:2048).
//!
//! For each element (a log's address, and each topic), take keccak256(element)
//! and fold three 11-bit positions from byte pairs (0,1), (2,3), (4,5) into the
//! 2048-bit accumulator. The bloom is rendered big-endian: bit `i` of the
//! accumulator lives in byte `255 - i/8`, bit `i % 8` — matching geth's bloom9.
//! Receipt bloom = OR over its logs' elements.

use sha3::{Digest, Keccak256};

pub const BLOOM_BYTES: usize = 256;

#[derive(Clone, PartialEq, Eq)]
pub struct Bloom(pub [u8; BLOOM_BYTES]);

impl Default for Bloom {
    fn default() -> Self {
        Bloom([0u8; BLOOM_BYTES])
    }
}

impl Bloom {
    /// Fold one element (address or topic bytes) into the bloom.
    pub fn accrue(&mut self, element: &[u8]) {
        let hash: [u8; 32] = Keccak256::digest(element).into();
        for pair in 0..3 {
            let idx = (((hash[2 * pair] as u16) << 8) | hash[2 * pair + 1] as u16) & 0x7ff;
            let byte = BLOOM_BYTES - 1 - (idx as usize) / 8;
            self.0[byte] |= 1 << (idx % 8);
        }
    }

    /// OR another bloom into this one (receipt bloom = OR of log blooms).
    pub fn union(&mut self, other: &Bloom) {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            *a |= b;
        }
    }

    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }
}

/// Bloom over one log: address + every topic.
pub fn log_bloom(address: &[u8], topics: &[Vec<u8>]) -> Bloom {
    let mut b = Bloom::default();
    b.accrue(address);
    for t in topics {
        b.accrue(t);
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_bloom_is_zero() {
        assert_eq!(Bloom::default().to_hex(), format!("0x{}", "00".repeat(256)));
    }

    /// Golden vector derived from the publicly-known constant
    /// keccak256("") = c5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470.
    /// Byte pairs (c5,d2),(46,01),(86,f7) & 0x7ff → bits 1490, 1537, 1783:
    ///   1490 → byte 255-186=69, bit 2 (0x04)
    ///   1537 → byte 255-192=63, bit 1 (0x02)
    ///   1783 → byte 255-222=33, bit 7 (0x80)
    #[test]
    fn golden_empty_element() {
        let mut b = Bloom::default();
        b.accrue(b"");
        let mut expected = [0u8; BLOOM_BYTES];
        expected[69] = 0x04;
        expected[63] = 0x02;
        expected[33] = 0x80;
        assert_eq!(b.0, expected);
    }

    #[test]
    fn at_most_three_bits_per_element_and_or_composition() {
        let mut b = Bloom::default();
        b.accrue(&[0xab; 20]);
        let ones: u32 = b.0.iter().map(|x| x.count_ones()).sum();
        assert!((1..=3).contains(&ones));

        // union == accruing both elements into one bloom
        let mut b2 = Bloom::default();
        b2.accrue(&[0xcd; 32]);
        let mut unioned = b.clone();
        unioned.union(&b2);
        let mut direct = Bloom::default();
        direct.accrue(&[0xab; 20]);
        direct.accrue(&[0xcd; 32]);
        assert_eq!(unioned.0, direct.0);
    }

    #[test]
    fn log_bloom_covers_address_and_topics() {
        let bloom = log_bloom(&[0x11; 20], &[vec![0x22; 32], vec![0x33; 32]]);
        // Each element contributes its bits; the union of singles must equal it.
        let mut expected = Bloom::default();
        expected.accrue(&[0x11; 20]);
        expected.accrue(&[0x22; 32]);
        expected.accrue(&[0x33; 32]);
        assert_eq!(bloom.0, expected.0);
    }
}
