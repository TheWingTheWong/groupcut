//! Parallel two-phase hash aggregation for
//!
//! ```sql
//! SELECT key, AGG(v) FROM t GROUP BY key ORDER BY AGG(v) DESC LIMIT k
//! ```
//!
//! Phase 1 (local): threads pull morsels of rows and aggregate them into a
//! small, cache-resident pre-aggregation table. When the table fills, every
//! entry is flushed as a partial aggregate into one of `2^partition_bits`
//! per-thread buffers, chosen by the high bits of the key's hash, and the
//! table is reset. This is the radix-partitioned design DuckDB uses: phase 1
//! is a streaming pass that never misses cache on a large table, and repeated
//! keys inside a morsel collapse before they are written out.
//!
//! Phase 2 (merge): partitions are handed out to workers. A worker
//! aggregates one partition's partials from all threads into a single table,
//! extracts that partition's top k, and folds it into a shared top-k heap.
//! Every group lives in exactly one partition, so per-partition answers
//! combine exactly.
//!
//! The baseline (`Strategy::Full`) merges every partition. That is where the
//! time goes on high-cardinality data, and nearly all of it is wasted: only
//! k groups survive the LIMIT.
//!
//! `Strategy::Prune` keeps a sketch while flushing. Every partial written to
//! partition `p` is also added into one of `2^slot_bits` counters of `p`,
//! chosen by hash bits that are disjoint from the partition bits. The flush
//! touches each partial anyway, so the sketch costs one add per partial. Summed over threads, a counter is an
//! upper bound on the final value of every group that maps to it (see
//! `agg.rs` for why this holds for COUNT, SUM with negative values, and MAX).
//! The partition's bound is its largest counter.
//!
//! Workers then take partitions in order of decreasing bound and keep a
//! shared threshold `tau`, the k-th best value found so far. A partition whose
//! bound is below `tau` cannot contain an answer and is skipped without being
//! merged; because partitions are visited in bound order, once one is skipped
//! all later ones are too. With `filter: true` the same test is applied per
//! slot inside partitions that do get merged: entries in a slot whose counter
//! is below `tau` are not inserted into the merge table at all.
//!
//! The result is exact. The pruning tests use strict `<`, so a group that
//! could tie the k-th value (and win on key order) is never skipped.

use crate::agg::{AggKind, AggOp, Count, Max, Sum};
use crate::hash::{mix, Layout};
use crate::table::AggTable;
use crate::topk::{Entry, TopK};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Strategy {
    /// Merge every partition, then take the top k.
    Full,
    /// Skip partitions (and, with `filter`, slots) whose sketch bound is
    /// below the running k-th value. `slot_bits = 0` uses one counter per
    /// partition: the partition total.
    Prune { slot_bits: u32, filter: bool },
}

impl Strategy {
    pub fn label(&self) -> String {
        match *self {
            Strategy::Full => "full merge".into(),
            Strategy::Prune { slot_bits: 0, filter: false } => "partition totals".into(),
            Strategy::Prune { slot_bits, filter: false } => format!("slots ({})", 1 << slot_bits),
            Strategy::Prune { slot_bits, filter: true } => format!("slots ({}) + filter", 1 << slot_bits),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub threads: usize,
    pub partition_bits: u32,
    /// log2 of the pre-aggregation table's bucket count (flushed at 5/8 load).
    pub preagg_bits: u32,
    pub strategy: Strategy,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4),
            partition_bits: 8,
            preagg_bits: 14,
            strategy: Strategy::Prune { slot_bits: 4, filter: true },
        }
    }
}

/// What happened to one partition in phase 2.
#[derive(Clone, Copy, Debug, Default)]
pub struct PartitionInfo {
    /// Sketch upper bound (0 / `i64::MIN` for `Full`, which has no sketch).
    pub bound: i64,
    /// Partial aggregates flushed to this partition by all threads.
    pub entries: u64,
    pub merged: bool,
    /// Entries skipped by the slot filter while merging.
    pub filtered: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub rows: u64,
    pub partitions: u64,
    /// Partial aggregates flushed by phase 1: the input size of the merge.
    pub local_entries: u64,
    pub partitions_merged: u64,
    pub partitions_pruned: u64,
    /// Entries inserted into merge tables.
    pub entries_merged: u64,
    /// Entries skipped by the per-slot filter.
    pub entries_filtered: u64,
    /// Distinct groups materialized in merge tables (all groups for `Full`).
    pub groups_merged: u64,
    pub local: Duration,
    pub sketch: Duration,
    pub merge: Duration,
    pub total: Duration,
    pub detail: Vec<PartitionInfo>,
}

impl Stats {
    /// Fraction of merge-phase input that was never inserted anywhere.
    pub fn entries_skipped_frac(&self) -> f64 {
        if self.local_entries == 0 {
            0.0
        } else {
            1.0 - self.entries_merged as f64 / self.local_entries as f64
        }
    }
}

/// Run the query. Returns `(key, value)` pairs, best first.
pub fn top_k(keys: &[u64], vals: &[i64], agg: AggKind, k: usize, cfg: &Config) -> (Vec<(u64, i64)>, Stats) {
    match agg {
        AggKind::Count => run::<Count>(keys, vals, k, cfg),
        AggKind::Sum => run::<Sum>(keys, vals, k, cfg),
        AggKind::Max => run::<Max>(keys, vals, k, cfg),
    }
}

fn run<A: AggOp>(keys: &[u64], vals: &[i64], k: usize, cfg: &Config) -> (Vec<(u64, i64)>, Stats) {
    assert_eq!(keys.len(), vals.len(), "keys and values must have the same length");
    let start = Instant::now();
    let (slot_bits, prune, filter) = match cfg.strategy {
        Strategy::Full => (0, false, false),
        Strategy::Prune { slot_bits, filter } => (slot_bits, true, filter),
    };
    let layout = Layout::new(cfg.partition_bits, slot_bits);
    let parts = layout.partitions();
    let slots = layout.slots();
    let threads = cfg.threads.max(1);
    let mut stats = Stats { rows: keys.len() as u64, partitions: parts as u64, ..Default::default() };

    // ---- Phase 1: pre-aggregate, flush partials into partitions ----------
    let t = Instant::now();
    // Morsel-driven: threads pull fixed-size row ranges from a shared cursor,
    // so a slow core (efficiency cores, a preempted thread) takes fewer
    // morsels instead of holding up the phase.
    const MORSEL: usize = 1 << 16;
    let cursor = AtomicUsize::new(0);
    let preagg_cap = 1usize << cfg.preagg_bits.clamp(4, 24);
    let locals: Vec<Local> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let cursor = &cursor;
                s.spawn(move || {
                    let mut local = Local {
                        bufs: (0..parts).map(|_| Vec::new()).collect(),
                        sketch: if prune { vec![A::BOUND_ZERO; parts * slots] } else { Vec::new() },
                    };
                    let mut table = AggTable::fixed(preagg_cap);
                    loop {
                        let lo = cursor.fetch_add(MORSEL, Relaxed);
                        if lo >= keys.len() {
                            break;
                        }
                        let hi = (lo + MORSEL).min(keys.len());
                        for (&key, &v) in keys[lo..hi].iter().zip(&vals[lo..hi]) {
                            table.upsert(key, mix(key), A::init(v), |a| A::update(a, v));
                            if table.is_full() {
                                local.flush::<A>(&mut table, layout, prune);
                            }
                        }
                    }
                    local.flush::<A>(&mut table, layout, prune);
                    local
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    stats.local = t.elapsed();

    let mut detail: Vec<PartitionInfo> = (0..parts)
        .map(|p| PartitionInfo { entries: locals.iter().map(|l| l.bufs[p].len() as u64).sum(), ..Default::default() })
        .collect();
    stats.local_entries = detail.iter().map(|d| d.entries).sum();

    if k == 0 {
        stats.partitions_pruned = parts as u64;
        stats.total = start.elapsed();
        stats.detail = detail;
        return (Vec::new(), stats);
    }

    // ---- Sketch: combine per-thread counters, order partitions by bound ----
    let t = Instant::now();
    let (sketch, order): (Vec<i64>, Vec<usize>) = if prune {
        let mut sketch = vec![A::BOUND_ZERO; parts * slots];
        for l in &locals {
            for (g, &c) in sketch.iter_mut().zip(&l.sketch) {
                A::bound_combine(g, c);
            }
        }
        for (p, d) in detail.iter_mut().enumerate() {
            d.bound = sketch[p * slots..(p + 1) * slots].iter().copied().max().unwrap();
        }
        let mut order: Vec<usize> = (0..parts).collect();
        order.sort_by(|&a, &b| detail[b].bound.cmp(&detail[a].bound));
        (sketch, order)
    } else {
        (Vec::new(), (0..parts).collect())
    };
    stats.sketch = t.elapsed();

    // ---- Phase 2: merge, with pruning --------------------------------------
    let t = Instant::now();
    let next = AtomicUsize::new(0);
    // k-th best value so far; i64::MIN until k groups are known. Only grows.
    let tau = AtomicI64::new(i64::MIN);
    let global = Mutex::new(TopK::new(k));
    // Size merge tables for a typical partition so most never grow.
    let est_groups = (stats.local_entries as usize / parts.max(1)).max(16);

    struct WorkerOut {
        touched: Vec<(usize, bool, u64)>, // (partition, merged, filtered)
        merged: u64,
        groups: u64,
    }

    let outs: Vec<WorkerOut> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (locals, detail, sketch, order) = (&locals, &detail, &sketch, &order);
                let (next, tau, global) = (&next, &tau, &global);
                s.spawn(move || {
                    let mut out = WorkerOut { touched: Vec::new(), merged: 0, groups: 0 };
                    let mut table = AggTable::with_capacity(est_groups);
                    let mut local_top = TopK::new(k);
                    loop {
                        let i = next.fetch_add(1, Relaxed);
                        if i >= parts {
                            break;
                        }
                        let p = order[i];
                        let thr = tau.load(Acquire);
                        if prune && detail[p].bound < thr {
                            out.touched.push((p, false, 0));
                            continue;
                        }
                        table.clear();
                        let mut filtered = 0u64;
                        if filter {
                            let row = &sketch[p * slots..(p + 1) * slots];
                            for l in locals {
                                for &(key, partial) in &l.bufs[p] {
                                    let h = mix(key);
                                    if row[layout.slot(h)] < thr {
                                        filtered += 1;
                                    } else {
                                        table.upsert(key, h, partial, |a| A::merge(a, partial));
                                    }
                                }
                            }
                        } else {
                            for l in locals {
                                for &(key, partial) in &l.bufs[p] {
                                    table.upsert(key, mix(key), partial, |a| A::merge(a, partial));
                                }
                            }
                        }
                        out.merged += detail[p].entries - filtered;
                        out.groups += table.len() as u64;
                        out.touched.push((p, true, filtered));

                        table.for_each(|key, val| {
                            if val >= thr {
                                local_top.push(Entry { key, val });
                            }
                        });
                        if !local_top.is_empty() {
                            let mut g = global.lock().unwrap();
                            for e in local_top.drain() {
                                g.push(e);
                            }
                            if let Some(th) = g.threshold() {
                                tau.store(th, Release);
                            }
                        }
                    }
                    out
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    stats.merge = t.elapsed();
    drop(locals);

    for o in &outs {
        stats.entries_merged += o.merged;
        stats.groups_merged += o.groups;
        for &(p, merged, filtered) in &o.touched {
            detail[p].merged = merged;
            detail[p].filtered = filtered;
            stats.entries_filtered += filtered;
            if merged {
                stats.partitions_merged += 1;
            } else {
                stats.partitions_pruned += 1;
            }
        }
    }
    let result = global.into_inner().unwrap().into_sorted().into_iter().map(|e| (e.key, e.val)).collect();
    stats.total = start.elapsed();
    stats.detail = detail;
    (result, stats)
}

/// One thread's phase-1 output.
struct Local {
    /// Partial aggregates per partition.
    bufs: Vec<Vec<(u64, i64)>>,
    /// `parts * slots` bound counters (empty when not pruning).
    sketch: Vec<i64>,
}

impl Local {
    #[inline(never)]
    fn flush<A: AggOp>(&mut self, table: &mut AggTable, layout: Layout, sketching: bool) {
        let slots = layout.slots();
        table.for_each(|key, partial| {
            let h = mix(key);
            let p = layout.partition(h);
            unsafe { self.bufs.get_unchecked_mut(p) }.push((key, partial));
            if sketching {
                let c = unsafe { self.sketch.get_unchecked_mut(p * slots + layout.slot(h)) };
                A::bound_combine(c, A::bound_contrib(partial));
            }
        });
        table.clear();
    }
}

/// The sketch exactly as a run with these parameters builds it, exposed so
/// tests can check the invariant directly: every group's final value is at
/// most the counter of its slot. Single-threaded, `preagg_bits` as in
/// `Config`.
pub fn sketch_bounds(keys: &[u64], vals: &[i64], agg: AggKind, preagg_bits: u32, layout: Layout) -> Vec<i64> {
    fn go<A: AggOp>(keys: &[u64], vals: &[i64], preagg_bits: u32, layout: Layout) -> Vec<i64> {
        let parts = layout.partitions();
        let mut local = Local {
            bufs: (0..parts).map(|_| Vec::new()).collect(),
            sketch: vec![A::BOUND_ZERO; parts * layout.slots()],
        };
        let mut table = AggTable::fixed(1 << preagg_bits.clamp(4, 24));
        for (&key, &v) in keys.iter().zip(vals) {
            table.upsert(key, mix(key), A::init(v), |a| A::update(a, v));
            if table.is_full() {
                local.flush::<A>(&mut table, layout, true);
            }
        }
        local.flush::<A>(&mut table, layout, true);
        local.sketch
    }
    match agg {
        AggKind::Count => go::<Count>(keys, vals, preagg_bits, layout),
        AggKind::Sum => go::<Sum>(keys, vals, preagg_bits, layout),
        AggKind::Max => go::<Max>(keys, vals, preagg_bits, layout),
    }
}
