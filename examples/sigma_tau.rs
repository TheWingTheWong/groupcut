//! The sizing ratio for the synthetic workloads, plus a "dominant key"
//! workload that separates the two ways of stating it.
//!
//! A slot can be skipped once its counter is below τ, the k-th best value.
//! The winners' own slots are merged anyway, so what has to be spread thin is
//! the *tail* mass: Σ_tail = Σ − (sum of the top k values). Spread evenly over
//! P × S counters, a typical counter is Σ_tail / (P × S), so pruning needs
//! P × S > Σ_tail / τ. (For COUNT and SUM. MAX counters hold a maximum, not a
//! sum, and are below τ in every slot without a winner.)
//!
//!     cargo run --release --example sigma_tau > bench/sizing-local.json

use groupcut::engine::{top_k, Config, Strategy};
use groupcut::gen::{rank_key, zipf_rows};
use groupcut::{reference, AggKind};

fn ratio(keys: &[u64], vals: &[i64]) -> (f64, f64, i64) {
    let top = reference::top_k(keys, vals, AggKind::Count, 10);
    let sigma = keys.len() as f64;
    let tail = sigma - top.iter().map(|t| t.1 as f64).sum::<f64>();
    (sigma, tail, top[9].1)
}

fn main() {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    println!("{{\"synthetic\": {{");
    let skews = [0.0, 0.5, 0.8, 1.0, 1.2, 1.5];
    for (i, s) in skews.iter().enumerate() {
        // Same rows, keys and seed as `groupcut bench`.
        let (keys, vals) = zipf_rows(20_000_000, 4_000_000, *s, 1, 100, 7, threads);
        let (sigma, tail, tau) = ratio(&keys, &vals);
        let comma = if i + 1 < skews.len() { "," } else { "" };
        println!(
            "  \"{s:?}\": {{\"sigma\": {sigma}, \"tail\": {tail}, \"tau\": {tau}, \"sigma_over_tau\": {:.1}, \"tail_over_tau\": {:.1}}}{comma}",
            sigma / tau as f64,
            tail / tau as f64
        );
    }
    println!("}},");

    // One key with 10M rows, ten keys with 100 rows, 200K singletons.
    // Σ/τ ≈ 102,000 but Σ_tail/τ ≈ 2,000: the dominant key's mass sits in a
    // slot that is merged anyway.
    let mut keys: Vec<u64> = vec![rank_key(0); 10_000_000];
    for r in 1..=10 {
        keys.extend(std::iter::repeat(rank_key(r)).take(100));
    }
    keys.extend((0..200_000).map(|i| rank_key(1_000 + i)));
    let mut rng = groupcut::gen::Rng::new(3);
    for i in (1..keys.len()).rev() {
        keys.swap(i, rng.below(i as u64 + 1) as usize);
    }
    let vals = vec![1i64; keys.len()];
    let (sigma, tail, tau) = ratio(&keys, &vals);
    println!(
        "\"dominant\": {{\"sigma\": {sigma}, \"tail\": {tail}, \"tau\": {tau}, \"sigma_over_tau\": {:.1}, \"tail_over_tau\": {:.1}, \"runs\": [",
        sigma / tau as f64,
        tail / tau as f64
    );
    let setups = [
        (8u32, 0u32),
        (8, 2),
        (8, 3),
        (8, 4),
        (10, 4),
        (12, 4),
        (8, 8),
    ];
    for (i, &(pb, sb)) in setups.iter().enumerate() {
        let cfg = Config {
            threads,
            partition_bits: pb,
            preagg_bits: 14,
            morsel_rows: 1 << 16,
            adaptive_preagg: true,
            strategy: Strategy::Prune {
                slot_bits: sb,
                filter: false,
            },
        };
        let (got, st) = top_k(&keys, &vals, AggKind::Count, 10, &cfg);
        assert_eq!(got, reference::top_k(&keys, &vals, AggKind::Count, 10));
        let comma = if i + 1 < setups.len() { "," } else { "" };
        println!(
            "  {{\"partitions\": {}, \"slots\": {}, \"partitions_merged\": {}, \"skipped\": {:.4}}}{comma}",
            1u32 << pb,
            1u32 << sb,
            st.partitions_merged,
            st.entries_skipped_frac()
        );
    }
    println!("]}}}}");
}
