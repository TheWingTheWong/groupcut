//! Randomized differential tests: every strategy, aggregate, thread count and
//! bit layout must return exactly what the naive HashMap + sort reference
//! returns, including tie order. Plus direct checks of the bound invariant.

use groupcut::engine::{sketch_bounds, top_k, Config, Strategy};
use groupcut::gen::{rank_key, Rng, Zipf};
use groupcut::hash::{mix, Layout};
use groupcut::{reference, AggKind};
use std::collections::HashMap;

const AGGS: [AggKind; 3] = [AggKind::Count, AggKind::Sum, AggKind::Max];

/// A random workload with deliberately awkward shapes mixed in.
fn workload(rng: &mut Rng) -> (Vec<u64>, Vec<i64>) {
    let rows = match rng.below(10) {
        0 => 0,
        1 => 1 + rng.below(5) as usize,
        _ => rng.below(30_000) as usize,
    };
    let max_groups = [10, 2_000, 40_000][rng.below(3) as usize];
    let groups = 1 + rng.below(max_groups) as usize;
    let s = [0.0, 0.5, 0.9, 1.2, 2.0][rng.below(5) as usize];
    let z = Zipf::new(groups, s);
    // Value ranges: all positive, mixed sign, all negative, and huge.
    let (lo, hi) = match rng.below(4) {
        0 => (1, 100),
        1 => (-50, 50),
        2 => (-1000, -1),
        _ => (-(1 << 40), 1 << 40),
    };
    let span = (hi - lo + 1) as u64;
    let mut keys = Vec::with_capacity(rows);
    let mut vals = Vec::with_capacity(rows);
    for _ in 0..rows {
        let r = z.sample(rng);
        // Sometimes use raw small integers (including 0, the table's empty
        // marker) and u64::MAX as keys instead of scrambled ones.
        let key = match rng.below(20) {
            0 => 0,
            1 => u64::MAX,
            _ if s == 2.0 => r as u64,
            _ => rank_key(r),
        };
        keys.push(key);
        vals.push(lo + rng.below(span) as i64);
    }
    (keys, vals)
}

fn random_strategy(rng: &mut Rng) -> Strategy {
    match rng.below(3) {
        0 => Strategy::Full,
        _ => Strategy::Prune {
            slot_bits: rng.below(7) as u32,
            filter: rng.below(2) == 1,
        },
    }
}

#[test]
fn every_configuration_matches_the_reference() {
    let mut rng = Rng::new(0x6772_6f75_7063_7574);
    let mut checked = 0;
    for case in 0..400 {
        let (keys, vals) = workload(&mut rng);
        let k = match rng.below(4) {
            0 => rng.below(3) as usize,
            1 => 10,
            2 => rng.below(200) as usize,
            _ => 100_000, // more than the number of groups
        };
        for agg in AGGS {
            let expect = reference::top_k(&keys, &vals, agg, k);
            for _ in 0..3 {
                let cfg = Config {
                    threads: 1 + rng.below(8) as usize,
                    partition_bits: rng.below(9) as u32,
                    preagg_bits: 4 + rng.below(10) as u32,
                    // 64..4096 rows, so phase 1 really runs on several threads.
                    morsel_rows: 64 << rng.below(7),
                    strategy: random_strategy(&mut rng),
                };
                let (got, stats) = top_k(&keys, &vals, agg, k, &cfg);
                assert_eq!(
                    got,
                    expect,
                    "case {case}: {agg:?} k={k} rows={} cfg={cfg:?}",
                    keys.len()
                );
                assert_eq!(
                    stats.partitions_merged + stats.partitions_pruned,
                    stats.partitions
                );
                if cfg.strategy == Strategy::Full && k > 0 {
                    assert_eq!(stats.partitions_pruned, 0);
                    assert_eq!(stats.entries_filtered, 0);
                }
                assert_eq!(
                    stats.entries_merged
                        + stats.entries_filtered
                        + stats
                            .detail
                            .iter()
                            .filter(|d| !d.merged)
                            .map(|d| d.entries)
                            .sum::<u64>(),
                    stats.local_entries,
                    "every partial is merged, filtered, or in a pruned partition"
                );
                checked += 1;
            }
        }
    }
    assert!(checked >= 3600);
}

#[test]
fn ties_are_broken_by_key_and_never_pruned_away() {
    // 5,000 groups with identical counts: the answer is the k smallest keys.
    // A slot filter that used <= instead of < would drop some of them.
    let mut keys = Vec::new();
    for round in 0..4 {
        for g in 0..5_000u64 {
            keys.push(if round % 2 == 0 { g } else { 4_999 - g });
        }
    }
    let vals = vec![1i64; keys.len()];
    for slot_bits in 0..=6 {
        for filter in [false, true] {
            let cfg = Config {
                threads: 4,
                partition_bits: 6,
                preagg_bits: 8,
                morsel_rows: 256,
                strategy: Strategy::Prune { slot_bits, filter },
            };
            let (got, _) = top_k(&keys, &vals, AggKind::Count, 25, &cfg);
            let expect: Vec<(u64, i64)> = (0..25).map(|g| (g, 4)).collect();
            assert_eq!(got, expect);
        }
    }
}

#[test]
fn sketch_counters_bound_every_group() {
    let mut rng = Rng::new(99);
    for _ in 0..60 {
        let (keys, vals) = workload(&mut rng);
        let layout = Layout::new(rng.below(7) as u32, rng.below(7) as u32);
        let preagg_bits = 4 + rng.below(8) as u32;
        for agg in AGGS {
            let sketch = sketch_bounds(&keys, &vals, agg, preagg_bits, layout);
            for (key, val) in reference::top_k(&keys, &vals, agg, usize::MAX) {
                let h = mix(key);
                let bound = sketch[layout.partition(h) * layout.slots() + layout.slot(h)];
                assert!(
                    val <= bound,
                    "{agg:?}: group {key} = {val} exceeds its slot bound {bound}"
                );
            }
        }
    }
}

/// Find a key whose hash lands in `partition` (and `slot`, if given).
fn key_in(layout: Layout, partition: usize, slot: Option<usize>, skip: &[u64]) -> u64 {
    (1u64..)
        .find(|&k| {
            let h = mix(k);
            layout.partition(h) == partition
                && slot.map_or(true, |s| layout.slot(h) == s)
                && !skip.contains(&k)
        })
        .unwrap()
}

#[test]
fn negative_sums_do_not_break_the_bound() {
    // Group `a` (total +1000) shares a slot with group `b` (total -5000).
    // Summing raw partials would give that slot a "bound" of -4000, below the
    // +100 of group `c` in another partition, and `a`, the right answer for
    // k = 1, would be pruned. Clamping each partial at zero keeps the slot's
    // bound at 1000. One thread makes partition order deterministic: `c`'s
    // partition (bound 100) is merged first and sets the threshold to 100.
    for slot_bits in [0, 3] {
        let layout = Layout::new(1, slot_bits);
        let a = key_in(layout, 0, Some(0), &[]);
        let b = key_in(layout, 0, Some(0), &[a]);
        let c = key_in(layout, 1, None, &[]);
        let keys = vec![a, b, b, c];
        let vals = vec![1000, -2500, -2500, 100];
        let expect = reference::top_k(&keys, &vals, AggKind::Sum, 1);
        assert_eq!(expect, vec![(a, 1000)]);
        for filter in [false, true] {
            for preagg_bits in [4, 10] {
                let cfg = Config {
                    threads: 1,
                    partition_bits: 1,
                    preagg_bits,
                    morsel_rows: 256,
                    strategy: Strategy::Prune { slot_bits, filter },
                };
                assert_eq!(
                    top_k(&keys, &vals, AggKind::Sum, 1, &cfg).0,
                    expect,
                    "slot_bits={slot_bits} filter={filter}"
                );
            }
        }
    }
}

#[test]
fn partitions_whose_bound_equals_the_threshold_are_merged() {
    // 40 groups with equal counts spread over 256 partitions: most
    // partitions hold one group, so their bound *equals* the k-th value. A
    // group there can still win on key order, so `bound < tau` must not
    // become `bound <= tau`.
    let keys: Vec<u64> = (0..40u64).flat_map(|g| [g, g, g]).collect();
    let vals = vec![1i64; keys.len()];
    let expect: Vec<(u64, i64)> = (0..25).map(|g| (g, 3)).collect();
    for slot_bits in [0, 4] {
        for filter in [false, true] {
            let cfg = Config {
                threads: 1,
                partition_bits: 8,
                preagg_bits: 8,
                morsel_rows: 256,
                strategy: Strategy::Prune { slot_bits, filter },
            };
            assert_eq!(top_k(&keys, &vals, AggKind::Count, 25, &cfg).0, expect);
        }
    }
}

#[test]
#[should_panic(expected = "SUM overflowed i64")]
fn sum_overflow_is_an_error_not_a_wrong_answer() {
    // Wrapping here would turn two huge negative partials into +5 and make
    // the clamped bound (0) lie. Overflow must fail loudly instead.
    let keys = vec![1, 1, 2];
    let vals = vec![i64::MIN, i64::MIN + 5, 3];
    let cfg = Config {
        threads: 1,
        partition_bits: 2,
        preagg_bits: 4,
        morsel_rows: 256,
        strategy: Strategy::Prune {
            slot_bits: 2,
            filter: true,
        },
    };
    top_k(&keys, &vals, AggKind::Sum, 1, &cfg);
}

#[test]
fn skewed_data_actually_prunes() {
    // A guard against pruning silently turning into a no-op.
    let z = Zipf::new(200_000, 1.1);
    let mut rng = Rng::new(5);
    let keys: Vec<u64> = (0..400_000).map(|_| rank_key(z.sample(&mut rng))).collect();
    let vals = vec![1i64; keys.len()];
    let cfg = Config {
        threads: 4,
        partition_bits: 8,
        preagg_bits: 12,
        morsel_rows: 256,
        strategy: Strategy::Prune {
            slot_bits: 4,
            filter: true,
        },
    };
    let (got, stats) = top_k(&keys, &vals, AggKind::Count, 10, &cfg);
    assert_eq!(got, reference::top_k(&keys, &vals, AggKind::Count, 10));
    assert!(
        stats.partitions_pruned > 200,
        "pruned only {} of 256",
        stats.partitions_pruned
    );
    assert!(stats.entries_skipped_frac() > 0.9);
}

#[test]
fn uniform_data_prunes_nothing_but_stays_exact() {
    // Flat distribution: every slot bound exceeds the k-th value, so the
    // sketch cannot help. The result must still be right.
    let keys: Vec<u64> = (0..200_000u64).map(|i| mix(i % 50_000)).collect();
    let vals = vec![1i64; keys.len()];
    let cfg = Config {
        threads: 4,
        partition_bits: 8,
        preagg_bits: 10,
        morsel_rows: 256,
        strategy: Strategy::Prune {
            slot_bits: 4,
            filter: true,
        },
    };
    let (got, stats) = top_k(&keys, &vals, AggKind::Count, 10, &cfg);
    assert_eq!(got, reference::top_k(&keys, &vals, AggKind::Count, 10));
    assert_eq!(stats.partitions_pruned, 0);
}

#[test]
fn results_do_not_depend_on_thread_count() {
    let mut rng = Rng::new(17);
    let (keys, vals) = {
        let z = Zipf::new(30_000, 0.8);
        let keys: Vec<u64> = (0..100_000).map(|_| rank_key(z.sample(&mut rng))).collect();
        let vals: Vec<i64> = (0..100_000).map(|_| rng.below(1000) as i64 - 300).collect();
        (keys, vals)
    };
    let mut answers: HashMap<AggKind, Vec<(u64, i64)>> = HashMap::new();
    for threads in 1..=12 {
        for agg in AGGS {
            let cfg = Config {
                threads,
                partition_bits: 5,
                preagg_bits: 9,
                morsel_rows: 256,
                strategy: Strategy::Prune {
                    slot_bits: 3,
                    filter: true,
                },
            };
            let got = top_k(&keys, &vals, agg, 50, &cfg).0;
            let prev = answers.entry(agg).or_insert_with(|| got.clone());
            assert_eq!(&got, prev, "{agg:?} threads={threads}");
        }
    }
}
