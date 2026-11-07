# groupcut

groupcut answers `SELECT key, AGG(v) ... GROUP BY key ORDER BY AGG(v) DESC LIMIT k`
without merging most of the groups. It is a parallel hash-aggregation engine
in Rust (no dependencies) that keeps a small sketch of upper bounds per hash
partition and skips every partition that cannot hold one of the top k. The
answers are exact.

On six hours of Wikipedia pageviews (34.6M rows, 18.3M distinct pages, top 10
by views), the sketch skips 242 of 256 partitions. The query runs **1.8-2.0x
faster** than a full merge on three separate GitHub Actions runners (481 → 253
ms on the first). The simpler bound from the post that inspired this, one
total per partition, skips **none** of the 256 partitions. It only starts
pruning at 4096 partitions, and even there it is 1.4-1.6x slower than slots
at 256. Every answer is checked against a naive `HashMap` + sort reference.

![Wikipedia benchmark: full merge vs partition totals vs slot sketches](assets/wiki.svg)

## What this adds to the Pivot post

Pivot's [*Can GROUP BY still be optimized after all these years?*](https://pivotlake.io/blog/know-your-limit-faster-top-k-aggregation/)
(October 2026) describes skipping partitions by their total and refining the
total into hashed "slots". It reports a single number, 1.6x on ClickBench Q16.
This repo contributes:

- **A real dataset where partition totals fail completely.** The 10th-best
  page has 81,265 views, but the smallest of the 256 partition totals is
  365,480. Each partition holds ~72,000 pages, and their long tail adds up.
- **A slot ablation.** At 256 partitions the effect is a threshold: 1-2 slots
  prune nothing, 4 slots leave 27 partitions, and 8 or more leave 11-13.
- **Bounds for SUM with negative values and for MAX**, and a per-slot filter
  inside the partitions that do get merged. Each correctness rule is backed
  by a test that fails when the rule is mutated.
- **A measured baseline.** It is a full radix-partitioned aggregation that
  bypasses pre-aggregation when it isn't reducing anything, plus DuckDB on the
  same data for scale.

## Quickstart

```sh
cargo test --release                                  # ~2 min cold build, then seconds
cargo run --release -- demo                           # 20M Zipf rows, all strategies vs a reference
scripts/fetch-wiki.sh                                 # 6 hourly pageview dumps, ~300 MB, CC0
cargo run --release -- wiki --check data/*.gz         # the headline query
cargo run --release -- wiki --partition-bits 12 data/*.gz
cargo run --release -- bench                          # synthetic sweeps (5-15 min)
python3 scripts/plot.py                               # re-render assets/ from bench/gha/run1
```

`.github/workflows/bench.yml` runs all of the above on three GitHub runners
in parallel. Its outputs are committed in `bench/gha/run{1,2,3}/`. First
runner, `wiki --reps 7 --check` (query time only; loading the gzipped dumps
takes another ~3 s):

```
  strategy                total ms     local    sketch     merge  speedup  merge x partitions   skipped
  full merge                 480.8     166.0       0.0     288.8    1.00x    1.00x  256/256        0.0%
  partition totals           498.3     189.8       0.0     287.2    0.96x    1.01x  256/256        0.0%
  slots (16)                 256.6     192.9       0.0      33.3    1.87x    8.68x   14/256       94.5%
  slots (16) + filter        252.5     192.8       0.0      30.4    1.90x    9.51x   14/256       96.0%
  18341211 distinct pages, 34558052 partial aggregates across 256 partitions; answers identical across
  strategies; all match the HashMap + sort reference (6705 ms)
```

## How it works

**Phase 1.** Threads pull 64K-row morsels from a shared cursor and aggregate
them in a 16K-bucket, cache-resident table. When the table is 5/8 full, every
entry is flushed as a *partial aggregate* into one of P per-thread buffers,
chosen by the top bits of the key's hash. If a flush shows the table cut the
rows by less than 10%, as on the pageview data where every (wiki, page)
appears once per file, the thread scatters the next 16 morsels straight into
the buffers, then probes again.

**Phase 2.** Each partition's partials from all threads are merged into one
hash table, and its local top k is folded into a shared heap. A group lives in
exactly one partition, so partitions are independent. With `LIMIT 10` almost
all of this work is wasted.

**Pruning.** While flushing, each partial is also added to one of 2^b
counters of its partition, chosen by the hash bits right after the partition
bits (a one-row Count-Min sketch per partition). A group's final value is at
most its slot's counter, so a partition's bound is its largest counter.
Workers take partitions in decreasing bound order and share τ, the k-th best
exact value found so far. A partition whose bound is below τ is skipped
unread, and so is every partition after it. With `filter`, partials whose own
slot counter is below τ are skipped inside merged partitions too.

With b = 0 the bound is the partition total. On the pageview data that is
always above τ. With 16 slots the median partition bound is 33,859, well below
τ = 81,265.

![Which partitions were merged](assets/partition-map.svg)

**Why the bound holds.** A group's value is a combination of its partials,
which can come from several threads and flushes. The sketch combines a
contribution per partial with an operator that can only overshoot:

| aggregate | contribution of a partial | combine |
|---|---|---|
| `COUNT(*)` | the partial | `+` (saturating) |
| `SUM(v)`, any sign | `max(partial, 0)` | `+` (saturating) |
| `MAX(v)` | the partial | `max` |

Without the clamp, a large negative group in the same slot could pull the
counter below a real value. Pruning tests use strict `<`: a group that ties τ
can still win on key order (ties break by smaller key). τ only grows (it is
written under the heap's lock), so a worker reading a stale τ prunes less,
never wrongly. SUM panics on `i64` overflow rather than wrapping, because a
wrapped partial would break the bound.

## Results

All from `bench/gha/` (GitHub Actions, Linux, 4 vCPU, medians of 5-7 runs).
Absolute times differ by up to 30% between runners. The ratios and partition
counts are stable.

**Wikipedia, by partition count** (first runner; the other two in
`bench/gha/run2,3/`):

| partitions | full merge | partition totals | slots (16) + filter |
|---|---|---|---|
| 256 | 481 ms | 498 ms, merged 256/256 | **253 ms**, merged 14 |
| 1024 | 522 ms | 550 ms, merged 1024/1024 | 330 ms, merged 12 |
| 4096 | 505 ms | 349 ms, merged 16 | 387 ms, merged 13 |

At 4096 partitions the totals are finally small enough to prune. By then, every
row is scattered across 16 times as many buffers, and the slot counters
(4096 × 16 × 8 bytes per thread) no longer fit in cache, which is why slots
lose there. Across the three runners, slots at 256 beat the full merge by
1.80-1.96x and the best partition-totals setup by 1.38-1.65x.

The sketch makes phase 1 ~16% slower (166 → 193 ms) because every partial
updates a counter. The merge saves ~260 ms. With pre-aggregation forced on
(`--adaptive 0`, the weaker baseline), the full merge takes 630 ms and the
speedup drops to 1.52x. The bypass helps every strategy, and makes the merge a
larger share of the query.

**DuckDB, for scale.** DuckDB 2.0, same runners, same data, 4 threads, table
already loaded (`duckdb-gha.txt`): 775-951 ms grouping by a precomputed
64-bit hash of (wiki, page), the same input groupcut gets, and 1.8-2.2 s
grouping by the two string columns. Its top 10 is identical. DuckDB is a
general engine (NULLs, 128-bit sums, spilling), so this places groupcut's
numbers in context. It is not a benchmark win.

**Synthetic, by skew.** 20M rows over 4M keys, Zipf(s), `COUNT(*)`, k = 10
(`results-gha.txt`):

| s | full merge | partition totals | slots (16) + filter |
|---|---|---|---|
| 0 (uniform) | 219 ms | merged 256 · 0.95x | merged 256 · 0.93x |
| 0.8 | 240 ms | merged 256 · 0.96x | merged 12 · 1.25x |
| 1.0 | 164 ms | merged 17 · 1.24x | merged 12 · 1.21x |
| 1.5 | 31 ms | merged 11 · 1.03x | merged 11 · 1.03x |

![Partitions merged and speedup by skew](assets/skew.svg)

Pre-aggregation collapses repeated keys here, so the merge is a smaller share
than on the pageview data and the gains are smaller. Below s ≈ 0.5 nothing
can be pruned and the sketch costs 4-13% at 4 threads. Single-threaded, best
of 15, the measured overhead ranges from -1% to +8% across the three runners.
At s = 0.8 the synthetic data behaves like the pageviews: totals prune
nothing, slots prune 95%.

**Slots and k.** At s = 0.8, 1-2 slots merge 255-256 partitions, 4 merge 27,
and 8-256 merge 11-13 (`assets/slots.svg`). Time is flat from 8 to 32 slots,
then rises slowly as the counters outgrow cache, so 16 is the default. k = 1
and k = 10 skip 95-98% of partials. k = 100 skips 57% (80% with the filter).
At k = 1000, τ is below every partition's bound and nothing is pruned.

## Tests

`cargo test --release`:

- **Differential:** 3,600 random runs (400 workloads × 3 aggregates × 3
  configs) checked exactly against the reference, tie order included. Configs
  vary threads 1-8, partitions 1-256, slots 1-64, pre-aggregation size,
  morsels of 64-4K rows (so phase 1 really runs on several threads), the
  bypass, and the filter. Workloads include empty input, k = 0, k above the
  group count, all-negative and ±2^40 values, and the keys `0` and
  `u64::MAX`.
- **The bound invariant**, checked directly against true group values.
- **One test per rule, each verified to fail under mutation:**
  - the SUM clamp: a winner shares a slot with a large negative group;
  - strict `<` for partitions: single-group partitions whose bound equals τ;
  - strict `<` for the filter: 5,000 tied groups;
  - checked SUM: overflow must panic;
  - the pre-aggregation bypass with mixed-sign values.

CI runs fmt, clippy `-D warnings`, the tests and a demo on Linux and macOS.

## Limitations

- Only `ORDER BY agg DESC` with COUNT, SUM or MAX. `COUNT(*) ASC` has no
  useful upper bound. One aggregate, `i64` values, no NULLs, no spilling.
- Keys are 64-bit hashes of the grouping columns. A collision would merge two
  groups (chance ~10^-5 at 18M keys); the `wiki` command re-reads the input
  and flags any collision among the reported winners.
- SUM partials combine in a thread-dependent order, so an intermediate
  overflow can panic even when the final sum fits.
- τ is unknown until the first partition finishes, so the first T partitions
  (T = threads) are always merged, and merged counts vary by 1-3 between runs.
  Seeding τ from the k-th largest partial of any flush (valid for COUNT, MAX
  and non-negative SUM) would remove that floor. It isn't implemented.
- Results come from shared CI runners and one laptop (`bench/m2/`, older
  code, heavy background load). There is no ClickBench run.

## References

- Gilad Kleinman, [*Can GROUP BY still be optimized after all these years?*](https://pivotlake.io/blog/know-your-limit-faster-top-k-aggregation/),
  Pivot blog, 27 Oct 2026 ([source](https://github.com/pivotlake/pivot/)).
- Leis, Boncz, Kemper, Neumann, *Morsel-Driven Parallelism*, SIGMOD 2014.
- Raasveldt, [*Parallel Grouped Aggregation in DuckDB*](https://duckdb.org/2022/03/07/aggregate-hashtable.html), 2022.
- Fagin, Lotem, Naor, *Optimal Aggregation Algorithms for Middleware*, PODS
  2001 (the threshold algorithm: stop once the bound falls below the k-th
  value).
- Cormode, Muthukrishnan, *An Improved Data Stream Summary: The Count-Min
  Sketch and its Applications*, 2005.
- Data: [Wikimedia pageview dumps](https://dumps.wikimedia.org/other/pageviews/),
  2026-11-01 12:00-17:00 UTC, CC0.

## License

MIT
