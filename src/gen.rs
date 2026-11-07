//! Deterministic synthetic data: a small PRNG and skewed key distributions.

use crate::hash::mix;

/// SplitMix64. Fast, seedable, and good enough for workload generation.
#[derive(Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x5851_f42d_4c95_7f2d)
    }

    #[inline(always)]
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (n > 0), via Lemire's multiply-shift.
    #[inline(always)]
    pub fn below(&mut self, n: u64) -> u64 {
        ((self.next_u64() as u128 * n as u128) >> 64) as u64
    }

    /// Uniform in `[0, 1)`.
    #[inline(always)]
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }
}

/// Zipf over ranks `0..n` with exponent `s`: P(rank r) ∝ 1 / (r + 1)^s.
/// `s = 0` is uniform. Sampling inverts a precomputed CDF.
pub struct Zipf {
    cdf: Vec<f64>,
}

impl Zipf {
    pub fn new(n: usize, s: f64) -> Self {
        assert!(n > 0);
        let mut cdf = Vec::with_capacity(n);
        let mut acc = 0.0;
        for r in 0..n {
            acc += 1.0 / ((r + 1) as f64).powf(s);
            cdf.push(acc);
        }
        let total = acc;
        for c in &mut cdf {
            *c /= total;
        }
        *cdf.last_mut().unwrap() = 1.0;
        Zipf { cdf }
    }

    #[inline]
    pub fn sample(&self, rng: &mut Rng) -> usize {
        let u = rng.unit();
        self.cdf
            .partition_point(|&c| c <= u)
            .min(self.cdf.len() - 1)
    }

    pub fn len(&self) -> usize {
        self.cdf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cdf.is_empty()
    }
}

/// The key that rank `r` is stored under. Scrambled so that popular ranks are
/// not clustered in key space (they would be anyway after hashing, but the
/// workload should not depend on that).
#[inline]
pub fn rank_key(r: usize) -> u64 {
    mix(r as u64 ^ 0x2545_f491_4f6c_dd1d)
}

/// Generate `rows` rows of `(key, value)` with Zipf-distributed keys over
/// `groups` ranks and values uniform in `lo..=hi`. Runs on `threads` threads;
/// output is identical for any thread count.
pub fn zipf_rows(
    rows: usize,
    groups: usize,
    s: f64,
    lo: i64,
    hi: i64,
    seed: u64,
    threads: usize,
) -> (Vec<u64>, Vec<i64>) {
    let z = Zipf::new(groups, s);
    let mut keys = vec![0u64; rows];
    let mut vals = vec![0i64; rows];
    const BLOCK: usize = 1 << 16;
    let span = (hi - lo + 1) as u64;
    let blocks: Vec<(usize, &mut [u64], &mut [i64])> = keys
        .chunks_mut(BLOCK)
        .zip(vals.chunks_mut(BLOCK))
        .enumerate()
        .map(|(i, (k, v))| (i, k, v))
        .collect();
    let per = blocks.len().div_ceil(threads.max(1)).max(1);
    let mut blocks = blocks;
    std::thread::scope(|s| {
        while !blocks.is_empty() {
            let take = per.min(blocks.len());
            let mine: Vec<_> = blocks.drain(..take).collect();
            let z = &z;
            s.spawn(move || {
                for (bi, ks, vs) in mine {
                    // Seed per block so output is independent of thread count.
                    let mut rng = Rng::new(seed.wrapping_mul(0x1000_0000_01b3) ^ bi as u64);
                    for (k, v) in ks.iter_mut().zip(vs.iter_mut()) {
                        *k = rank_key(z.sample(&mut rng));
                        *v = lo + rng.below(span) as i64;
                    }
                }
            });
        }
    });
    (keys, vals)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zipf_head_frequency_matches_theory() {
        let n = 1000;
        let s = 1.0;
        let z = Zipf::new(n, s);
        let h: f64 = (1..=n).map(|r| 1.0 / (r as f64).powf(s)).sum();
        let mut rng = Rng::new(11);
        let draws = 200_000;
        let mut c0 = 0;
        let mut c9 = 0;
        for _ in 0..draws {
            match z.sample(&mut rng) {
                0 => c0 += 1,
                9 => c9 += 1,
                _ => {}
            }
        }
        let p0 = 1.0 / h;
        let p9 = 1.0 / (10.0 * h);
        assert!((c0 as f64 / draws as f64 - p0).abs() < 0.01, "{c0}");
        assert!((c9 as f64 / draws as f64 - p9).abs() < 0.005, "{c9}");
    }

    #[test]
    fn generation_is_independent_of_thread_count() {
        let a = zipf_rows(300_000, 5000, 1.1, -3, 9, 42, 1);
        let b = zipf_rows(300_000, 5000, 1.1, -3, 9, 42, 7);
        assert_eq!(a, b);
        assert!(a.1.iter().all(|&v| (-3..=9).contains(&v)));
    }
}
