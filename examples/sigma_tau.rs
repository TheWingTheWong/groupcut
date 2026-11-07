//! Σ/τ for the synthetic workloads used by `groupcut bench`: the total of
//! the aggregate divided by the k-th best value (k = 10). The sketch can only
//! prune once partitions × slots is comfortably above this number, because a
//! typical slot counter is Σ / (partitions × slots) and must fall below τ.
//!
//!     cargo run --release --example sigma_tau

use groupcut::{gen::zipf_rows, reference, AggKind};

fn main() {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    println!("{{");
    let skews = [0.0, 0.5, 0.8, 1.0, 1.2, 1.5];
    for (i, s) in skews.iter().enumerate() {
        // Same rows, keys and seed as `groupcut bench`.
        let (keys, vals) = zipf_rows(20_000_000, 4_000_000, *s, 1, 100, 7, threads);
        let tau = reference::top_k(&keys, &vals, AggKind::Count, 10)[9].1;
        let sigma = keys.len() as f64;
        let comma = if i + 1 < skews.len() { "," } else { "" };
        println!(
            "  \"{s}\": {{\"sigma\": {sigma}, \"tau\": {tau}, \"sigma_over_tau\": {:.1}}}{comma}",
            sigma / tau as f64
        );
    }
    println!("}}");
}
