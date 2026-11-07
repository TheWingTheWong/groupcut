//! Key hashing and the bit layout that every phase agrees on.
//!
//! One 64-bit hash per key is cut into three independent fields:
//!
//! ```text
//!  63            64-pbits   64-pbits-sbits                       0
//!  [ partition bits ][   slot bits    ][ ...  table probe bits ... ]
//! ```
//!
//! The partition decides which radix partition a group lives in, the slot
//! decides which sketch counter of that partition it adds to, and the low bits
//! are used by the open-addressing tables. Because the fields do not overlap,
//! groups in one slot are spread evenly over the table, and each slot receives
//! a uniform share of the partition's groups.

/// Murmur3's 64-bit finalizer. A bijection, so distinct keys never collide
/// in the full 64-bit hash.
#[inline(always)]
pub fn mix(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^= k >> 33;
    k
}

/// The top `n` bits of `h` (0 when `n == 0`).
#[inline(always)]
pub fn top_bits(h: u64, n: u32) -> usize {
    if n == 0 {
        0
    } else {
        (h >> (64 - n)) as usize
    }
}

/// Bit layout for a run: how many bits select the partition and the slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub partition_bits: u32,
    pub slot_bits: u32,
}

impl Layout {
    pub fn new(partition_bits: u32, slot_bits: u32) -> Self {
        assert!(partition_bits <= 16, "at most 65536 partitions");
        assert!(slot_bits <= 12, "at most 4096 slots per partition");
        Layout {
            partition_bits,
            slot_bits,
        }
    }

    #[inline(always)]
    pub fn partitions(&self) -> usize {
        1 << self.partition_bits
    }

    #[inline(always)]
    pub fn slots(&self) -> usize {
        1 << self.slot_bits
    }

    #[inline(always)]
    pub fn partition(&self, h: u64) -> usize {
        top_bits(h, self.partition_bits)
    }

    #[inline(always)]
    pub fn slot(&self, h: u64) -> usize {
        // Shift the partition bits out, then take the next `slot_bits`.
        top_bits(h << self.partition_bits, self.slot_bits)
    }
}

/// FNV-1a over bytes followed by `mix`, used to turn string group keys
/// (Wikipedia page titles) into 64-bit keys.
pub fn hash_bytes(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            h ^= 0xff; // separator; 0xff never occurs in valid UTF-8
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        for &b in *p {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    mix(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_is_injective_on_a_sample() {
        let mut seen = std::collections::HashSet::new();
        for k in 0..100_000u64 {
            assert!(seen.insert(mix(k)));
        }
    }

    #[test]
    fn fields_do_not_overlap() {
        let l = Layout::new(3, 3);
        let h = 0b101_110u64 << 58 | 0xdead;
        assert_eq!(l.partition(h), 0b101);
        assert_eq!(l.slot(h), 0b110);
        let l0 = Layout::new(0, 0);
        assert_eq!(l0.partition(u64::MAX), 0);
        assert_eq!(l0.slot(u64::MAX), 0);
    }

    #[test]
    fn slots_are_roughly_uniform_within_a_partition() {
        let l = Layout::new(4, 4);
        let mut counts = vec![0u32; l.slots()];
        let mut n = 0;
        for k in 0..400_000u64 {
            let h = mix(k);
            if l.partition(h) == 7 {
                counts[l.slot(h)] += 1;
                n += 1;
            }
        }
        let expect = n as f64 / l.slots() as f64;
        for c in counts {
            assert!((c as f64 - expect).abs() < expect * 0.1, "{c} vs {expect}");
        }
    }
}
