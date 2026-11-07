# groupcut

On six hours of Wikipedia pageviews (34.6M rows, 18.3M distinct pages), the
obvious way to prune a top-10 `GROUP BY` is to skip any hash partition whose
total cannot reach the top 10. That skips **no partitions at all** at 256 or
1024 partitions, and only starts working at 4096, where the extra partitions
make phase 1 29% slower. Splitting each partition's total into 16 hashed slot
counters skips **242 of 256** partitions at the default size. On a 4-vCPU
GitHub Actions runner the query takes 352 ms, against 509 ms for a full merge
(1.45x) and 441 ms for the best partition-totals setup. All answers are
identical and checked against a naive reference.

![Wikipedia benchmark: full merge vs partition totals vs slot sketches](assets/wiki.svg)

groupcut is a small parallel aggregation engine in Rust (no dependencies) for
one query shape:

```sql
SELECT key, AGG(v) FROM t GROUP BY key ORDER BY AGG(v) DESC LIMIT k
```

It implements the radix-partitioned two-phase hash aggregation that analytical
databases use, plus the top-k pruning described in Pivot's post
[*Can GROUP BY still be optimized after all these years?*](https://pivotlake.io/blog/know-your-limit-faster-top-k-aggregation/)
(October 2026), and measures it on real and synthetic data. The post reports
one end-to-end number (1.6x on ClickBench Q16) and no ablation of the slot
refinement. This repo isolates that mechanism, extends the bounds to SUM with
negative values and to MAX, adds a per-slot filter, and checks every answer
against a naive reference.

What the numbers are, and are not:

- Query time only. Loading the six gzipped dumps (decompress, parse, hash the
  strings) takes 2.5 s and happens once, before timing.
- The baseline is this engine with pruning turned off. That isolates the
  technique, but it is not a comparison against a production database.
- For scale, DuckDB 2.0 on the same runner, same data, 4 threads
  (`bench/duckdb-gha.txt`): 1,717 ms grouping by the two string columns, and
  703 ms grouping by a precomputed 64-bit hash, which is the input groupcut
  gets. DuckDB is a general-purpose engine (NULLs, 128-bit sums, spilling),
  so this says where groupcut stands, not that it is "faster than DuckDB".

## Quickstart

```sh
cargo test --release                       # unit + randomized differential tests (~2 min cold build, then seconds)
cargo run --release -- demo                # 20M synthetic rows, every strategy, checked against a reference

scripts/fetch-wiki.sh                      # 6 hourly pageview dumps, ~300 MB, CC0
cargo run --release -- wiki --check data/*.gz        # the headline query (+ reference check)
cargo run --release -- wiki --partition-bits 12 data/*.gz   # the same at 4096 partitions
cargo run --release -- bench               # the synthetic sweeps behind the charts (4-15 min)
python3 scripts/plot.py                    # re-render assets/*.svg from bench/*-gha.json
uv run --with duckdb python scripts/duckdb_compare.py data/*.gz   # optional
```

Real output of `groupcut wiki --reps 7 --check data/*.gz` on the runner
(`bench/wiki-gha.txt`, produced by `.github/workflows/bench.yml` on the
current code):

```
SELECT domain_code, page_title, SUM(count_views) FROM pageviews GROUP BY domain_code, page_title ORDER BY 2 DESC LIMIT 10

   1.     639163  en.m Main_Page
   2.     421216  en Main_Page
   3.     158413  en.m Wikipedia:Featured_pictures
   4.     150185  de Wikipedia:Hauptseite
   5.     130321  en.m Tung_Tung_Tung_Sahur
   6.     109430  commons.m.m File:LinkedIn_icon.svg
   7.     105945  de.m Wikipedia:Hauptseite
   8.      90311  en.m Special:Search
   9.      86861  en Special:Search
  10.      81265  ja.m メインページ

  strategy                total ms     local    sketch     merge  speedup  merge x partitions   skipped
  full merge                 508.7     264.3       0.0     216.9    1.00x    1.00x  256/256        0.0%
  partition totals           540.1     298.1       0.0     214.4    0.94x    1.01x  256/256        0.0%
  slots (16)                 351.6     292.8       0.0      26.8    1.45x    8.10x   14/256       94.5%
  slots (16) + filter        353.4     292.1       0.0      24.8    1.44x    8.73x   14/256       96.0%
  18341211 distinct pages, 34558052 partial aggregates across 256 partitions; answers identical across strategies; all match the HashMap + sort reference (5949 ms)
```

`skipped` is the share of phase-1 output that was never inserted into a merge
table. Times are medians of 7 runs within one job. Between two runner jobs on
the same code the absolute times moved by ~25% (an earlier run: 681 ms full
merge, 442 ms pruned) while the ratio stayed at 1.45-1.54x, so read the
ratios and partition counts rather than the third digit. Phase 1 (`local`)
is 10-13% slower in all three pruning runs. Some of that is the extra counter
update per partial (on this data every row is a partial), but it does not
order cleanly by sketch size, so part of it is noise. The merge saves ~190 ms.

At other partition counts (`bench/wiki-gha-p10.txt`, `bench/wiki-gha-p12.txt`):

| partitions | full merge | partition totals | slots (16) + filter |
|---|---|---|---|
| 256 | 509 ms (local 264, merge 217) | 540 ms, merged 256/256 | **353 ms**, merged 14/256 |
| 1024 | 522 ms (local 309, merge 159) | 558 ms, merged 1024/1024 | 411 ms, merged 12/1024 |
| 4096 | 544 ms (local 342, merge 150) | 441 ms, merged 16/4096 | 449 ms, merged 13/4096 (a tie, within noise) |

More partitions make each partition total smaller, so at 4096 the totals
finally drop below τ. But every row is now scattered into 16 times as many
buffers, and phase 1 slows from 264 ms to 342 ms. The slot counters get the
same pruning without that cost. A full merge is also fastest at 256, which is
why 256 is the default (ClickHouse uses 256 buckets too).

## The idea

Parallel aggregation runs in two phases. In phase 1, every thread
pre-aggregates its rows and writes partial aggregates into P partitions chosen
by the high bits of the key's hash. In phase 2, each partition's partials from
all threads are merged into one hash table. Every group lives in exactly one
partition, so partitions can be merged independently and in parallel. With
`LIMIT 10`, nearly all of that merge work is thrown away.

**Partition totals.** While flushing partials, keep a running sum per
partition. No group can exceed the total of the partition it lives in. Merge
partitions in decreasing order of total while tracking τ, the 10th-best exact
value found so far. Once a partition's total is below τ, that partition and
every later one can be skipped unread.

This fails whenever a partition holds many moderate groups. In the Wikipedia
run the 10th-best page has 81,265 views, but the *smallest* partition total is
365,480: each partition holds ~72,000 pages, and their long tail adds up. No
partition is ever below τ.

**Slot counters.** Split each partition's counter into 2^b slots, chosen by
the hash bits right after the partition bits. A group's value is still at most
its slot's counter, so a partition's bound becomes its *largest* slot. The tail
is now divided 16 ways. With 16 slots the median partition bound falls to
33,859, below τ, and 242 of 256 partitions are skipped. Effectively this is a
one-row [Count-Min sketch](https://en.wikipedia.org/wiki/Count%E2%80%93min_sketch)
per partition, used for its one-sided error.

**Slot filter (not in the post).** The same counters work inside the partitions
that do get merged: a partial whose slot counter is below τ is not inserted at
all. The filter costs a hash and a lookup per partial, so it only runs in
partitions whose *smallest* slot counter is below τ. Elsewhere it could not
skip anything.

![Which partitions were merged](assets/partition-map.svg)

### Why the bound holds

A group's final value is a combination of its partials, which can come from
several threads and several flushes. The sketch combines a per-partial
*contribution* with an operator that can only overshoot:

| aggregate | final value of a group | contribution of a partial | combine |
|---|---|---|---|
| `COUNT(*)` | sum of partial counts | the partial | `+` |
| `SUM(v)`, any sign | sum of partial sums | `max(partial, 0)` | `+` |
| `MAX(v)` | max of partial maxima | the partial | `max` |

For SUM with negative values, summing raw partials would be wrong: a large
negative group in the same slot could cancel a positive one and push the
counter below a real value. Clamping each partial at zero gives a group total ≤
the sum of its positive partials ≤ the sum of every positive partial in its
slot. `tests/property.rs` checks the invariant directly
(`sketch_counters_bound_every_group`) and has a constructed case for the
cancellation (`negative_sums_do_not_break_the_bound`).

Every pruning test uses strict `<`. A group that could *tie* τ might still win
on key order (ties break by smaller key), so it is never skipped.

## Results

All numbers come from the scripts in this repo. `bench/*.txt` and
`bench/*.json` are the captured outputs, and the charts are rendered from the
JSON. Unless noted otherwise they are from the GitHub Actions runner (Linux,
4 vCPU, `bench/*-gha.*`). An older run on an 8 GB M2 laptop under heavy
unrelated load is kept in `bench/*-m2.*`. It shows the same partition counts,
much noisier times, and the same Wikipedia result: 3.02 s down to 2.12 s.

### Key skew

20M rows over 4M keys, Zipf exponent s, `COUNT(*)`, k = 10, 4 threads, 256
partitions (`bench/results-gha.txt`). Each cell gives partitions merged ·
merge-phase time · end-to-end speedup over the full merge:

| s | full merge | partition totals | slots (16) | slots (16) + filter |
|---|---|---|---|---|
| 0 (uniform) | 245 ms (67 merge) | 256/256 · 67 ms · 0.98x | 256/256 · 67 ms · 0.96x | 256/256 · 67 ms · 0.94x |
| 0.5 | 241 ms (67 merge) | 256/256 · 67 ms · 0.96x | 256/256 · 67 ms · 0.94x | 256/256 · 67 ms · 0.91x |
| 0.8 | 186 ms (59 merge) | 256/256 · 60 ms · 0.93x | 11/256 · 9 ms · 1.18x | 13/256 · 9 ms · 1.19x |
| 1.0 | 136 ms (33 merge) | 17/256 · 4 ms · 1.22x | 11/256 · 2 ms · 1.20x | 12/256 · 3 ms · 1.19x |
| 1.2 | 61 ms (10 merge) | 11/256 · 1 ms · 1.15x | 10/256 · 1 ms · 1.11x | 11/256 · 1 ms · 1.12x |
| 1.5 | 24 ms (1 merge) | 11/256 · 0 ms · 1.03x | 11/256 · 0 ms · 1.02x | 11/256 · 0 ms · 1.02x |

![Speedup by skew](assets/skew.svg)

- Below s ≈ 0.5 nothing can be pruned, because the top 10 are not far enough
  above the rest. No sketch changes that, and you pay 2-9%.
- At s = 0.8 the data looks like the Wikipedia data. Partition totals prune
  nothing; slots prune 95% of partials and the query is 1.18x faster.
- From s = 1.0 on, the heavy hitters are heavy enough that partition totals
  work too, and the three variants are within noise of each other.
- Synthetic end-to-end gains (1.1-1.2x) are smaller than on Wikipedia (1.45x).
  Pre-aggregation collapses repeated keys in phase 1, so less work is left
  for phase 2. In the pageview dumps every (wiki, page) appears once per file,
  so nothing collapses and the merge is 43% of the query.
- Merge-phase ratios at high skew (for example 33 ms down to 2 ms) are real,
  but they're ratios of millisecond timings. The partition counts and the
  absolute times are the numbers to read.

### Number of slots

At s = 0.8 the ablation (`assets/slots.svg`) has a sharp threshold:

- 1 or 2 slots merge 255-256 partitions;
- 4 slots merge 27;
- 8 slots merge 16;
- 16 to 256 slots merge 11-13.

End to end the best is at 8-16 slots (154-155 ms). By 256 slots it is back up
to 175 ms, because the per-thread counters (partitions × slots × 8 bytes)
stop fitting in cache. 16 is the default.

![Slot ablation](assets/slots.svg)

### k

At s = 1.0, k = 1 and k = 10 skip 95-98% of partials. k = 100 still skips 57%
(80% with the filter). By k = 1000, τ is so low that every partition has a slot
above it: nothing is pruned, and the sketch costs 5-6%.

### Overhead when nothing can be pruned

Uniform keys, one thread, best of 15 (`bench/results-gha.txt`):

| strategy | overhead vs full merge |
|---|---|
| partition totals | +4.4% |
| slots (16) | +5.2% |
| slots (16) + filter | +5.8% |

The cost is one counter update per partial during the phase-1 flush, plus
sorting the partition bounds. The filter turns itself off in partitions where
no slot is below τ.

### Partition counts vary with threads and between runs

τ is unknown until the first partition finishes, so the first T partitions
taken (T = worker threads) are always merged. Workers can also start a
partition before another one has raised τ far enough to rule it out. The
answer never changes, but the merged count does: 11-13 of 256 for 16 slots at
4 threads, and 15-16 at 8 threads. The first partitions taken have the
highest bounds, so they usually hold winners anyway.

Seeding τ before phase 2 would remove this floor. Within one flush every key
is distinct, so the k-th largest partial of any flush is a valid lower bound
on τ for COUNT, MAX, and SUM over non-negative values. It isn't implemented.

## What is implemented

- `src/engine.rs`: the two-phase engine.
  - **Phase 1** is morsel-driven: threads pull 64K-row ranges from a shared
    cursor ([Leis et al. 2014](https://db.in.tum.de/~leis/papers/morsels.pdf)),
    so a slow core takes fewer morsels instead of stalling the phase. Each
    thread aggregates into a fixed 16K-bucket table (256 KB, cache-resident).
    When the table reaches 5/8 load it flushes every entry as a partial into
    one of P per-thread buffers and resets. This is DuckDB's design for
    [parallel grouped aggregation](https://duckdb.org/2022/03/07/aggregate-hashtable.html).
    The sketch is updated during the flush, which touches every partial
    anyway.
  - **Sketch step:** per-thread counters are combined, and partitions get
    their bound (max slot) and their smallest slot, then are sorted by bound.
  - **Phase 2:** workers take partitions in bound order from an atomic index.
    Each one checks the shared τ (an `AtomicI64` that only grows), merges or
    skips, takes the partition's local top k, and folds it into a
    mutex-guarded global heap.
- `src/table.rs`: linear-probing table of `(u64, i64)` buckets. `0` marks an
  empty bucket, and the real key `0` lives in a side field, so a new table is
  a zeroed allocation (OS zero pages, no fill pass). Growable for phase 2,
  fixed for phase 1.
- `src/hash.rs`: one 64-bit hash per key (Murmur3 finalizer), split into
  disjoint fields: partition (top bits), slot (next bits), probe position
  (low bits).
- `src/agg.rs`: COUNT, SUM, MAX as monomorphized ops with their bound rules.
- `src/topk.rs`: bounded heap; order is value descending, then key ascending,
  so every query has exactly one correct answer.
- `src/wiki.rs`: streams `.gz` dumps through `gzip -dc` and hashes
  `(domain, title)` to 64-bit keys. When printing the answer it re-reads the
  files to recover the names and flags any key two different strings hash to,
  so a collision in the reported top k cannot go unnoticed.
- `src/main.rs`: `demo`, `bench` and `wiki`. Strategies are run interleaved
  round-robin, with the starting strategy rotated every round. An early version
  ran them in a fixed order, and whichever ran first in a round came out ~25%
  faster for no algorithmic reason.

## Testing

`cargo test --release` runs:

- **Differential test:** 400 random workloads × 3 aggregates × 3 random
  configurations (threads 1-8, 1-256 partitions, 1-64 slots, filter on/off,
  pre-aggregation tables of 16 to 8K buckets, morsels of 64 to 4K rows so
  phase 1 really is spread over threads), 3,600 runs in all. Every answer
  must equal a `HashMap` + sort reference exactly, including tie order. The
  workloads include empty input, k = 0, k larger than the number of groups,
  all-negative values, values near ±2^40, and the keys `0` and `u64::MAX`.
- **Bound invariant** checked directly against true group values.
- **Targeted cases**, each verified to fail when the rule it guards is
  mutated:
  - a winning group sharing a slot with a large negative group (fails without
    the clamp at zero);
  - single-group partitions whose bound equals τ exactly (fails if partition
    pruning uses `<=`);
  - 5,000 tied groups that must return the 25 smallest keys (fails if the slot
    filter uses `<=`).
- **SUM overflow** must panic rather than wrap. A wrapped partial would
  silently break the bound.
- **Liveness:** skewed data must actually prune (>200 of 256 partitions), and
  uniform data must prune nothing and still be exact.
- **Determinism** across 1-12 threads. The hash table, top-k heap, generator
  and Zipf sampler have their own unit tests.

CI runs fmt, clippy (`-D warnings`), the tests and a small demo on Linux and
macOS.

## Design decisions and trade-offs

- **Exact answers.** Approximate top-k (Space-Saving, Count-Min
  heavy hitters) is cheaper but gives a different answer. Here the sketch only
  decides what to skip. The merge itself is exact, so the answer is the same
  as with no pruning.
- **Sketch on flush, not per row.** Updating counters per input row would put
  an extra memory write in the hottest loop. Flushes see each distinct key in
  a morsel once. That saves a lot when keys repeat, and nothing when they
  don't: on the Wikipedia data every row is a partial, which is where the
  ~11% phase-1 cost comes from.
- **Visit partitions in bound order.** τ rises as fast as possible, and once
  one partition is skipped, every later one is too (bounds only decrease, τ
  only increases).
- **Fixed-width keys.** String keys are hashed to 64 bits up front. A real
  engine would keep the strings and compare them on collision. Here a
  collision would merge two groups. At 18M keys the chance of any collision
  is about 10^-5, and the answer path checks for one explicitly.
- **One τ for everyone.** Workers share τ through an atomic. A worker reads it
  once per partition, so it may use a slightly stale (lower) value. That is
  always safe; it just prunes a bit less.

## Limitations

- Only `ORDER BY agg DESC`. `ORDER BY COUNT(*) ASC` has no useful upper bound
  (every group is ≥ 1), and `MIN ... ASC` would need the mirror-image sketch.
  Neither is implemented.
- One aggregate per query, `i64` values, keys hashed to `u64`. SUM fails on
  `i64` overflow instead of widening to 128 bits. No spilling, no NULLs, no
  `HAVING`.
- The phase-1 pre-aggregation table always runs. On the Wikipedia data, keys
  never repeat within a morsel, so it reduces nothing. Detecting that and
  bypassing it would shrink phase 1 and make the merge, and therefore pruning,
  a larger share of the query.
- When the k-th value is not well separated from the bulk (uniform keys, or k
  in the thousands), nothing is pruned and you pay a small overhead.
- Benchmarks come from one shared CI runner and one loaded laptop. DuckDB is
  shown for scale on a single query and not tuned. The pruning itself is
  measured only against this engine with pruning turned off.
- SUM partials are combined in a thread-dependent order, so a SUM whose final
  value fits in `i64` can still overflow in an intermediate partial and panic.
  Strategies skip different merges, so one strategy can panic where another
  doesn't.

## References

- Gilad Kleinman, [*Can GROUP BY still be optimized after all these years?*](https://pivotlake.io/blog/know-your-limit-faster-top-k-aggregation/),
  Pivot blog, 27 Oct 2026. Source of the partition-total and slot pruning idea
  ([Pivot source](https://github.com/pivotlake/pivot/)).
- Leis, Boncz, Kemper, Neumann, *Morsel-Driven Parallelism*, SIGMOD 2014.
- Raasveldt, [*Parallel Grouped Aggregation in DuckDB*](https://duckdb.org/2022/03/07/aggregate-hashtable.html), 2022.
- Fagin, Lotem, Naor, *Optimal Aggregation Algorithms for Middleware*
  (the Threshold Algorithm), PODS 2001: the same "stop when the bound
  falls below the k-th value" idea, for top-k over sorted lists.
- Cormode, Muthukrishnan, *An Improved Data Stream Summary: The Count-Min
  Sketch and its Applications*, 2005.
- [ClickBench](https://benchmark.clickhouse.com/), where most queries have
  this shape.
- Data: [Wikimedia pageview dumps](https://dumps.wikimedia.org/other/pageviews/),
  2026-11-01 12:00-17:00 UTC, CC0.

## License

MIT
