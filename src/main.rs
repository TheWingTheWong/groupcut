use groupcut::engine::{top_k, Config, Stats, Strategy};
use groupcut::gen::zipf_rows;
use groupcut::wiki::{self, GroupBy};
use groupcut::{reference, AggKind};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Instant;

const USAGE: &str = "\
groupcut: exact top-k GROUP BY with sketch-based partition pruning

USAGE:
  groupcut demo  [--rows N] [--groups N] [--skew S] [--k K] [--threads T]
  groupcut bench [--rows N] [--groups N] [--reps R] [--threads T] [--out FILE]
  groupcut wiki  [--by page|title] [--agg sum|count|max] [--k K] [--reps R]
                 [--json FILE] FILES...

demo   one synthetic Zipf workload, every strategy, checked against a
       naive HashMap + sort reference
bench  the sweeps behind the README charts (skew, slots, k, partitions)
wiki   Wikimedia hourly pageview dumps (.gz or plain), see scripts/fetch-wiki.sh
";

struct Args {
    flags: Vec<(String, String)>,
    rest: Vec<String>,
}

impl Args {
    fn parse(argv: &[String]) -> Args {
        let mut flags = Vec::new();
        let mut rest = Vec::new();
        let mut i = 0;
        while i < argv.len() {
            if let Some(name) = argv[i].strip_prefix("--") {
                let v = argv
                    .get(i + 1)
                    .cloned()
                    .unwrap_or_else(|| die(&format!("--{name} needs a value")));
                flags.push((name.to_string(), v));
                i += 2;
            } else {
                rest.push(argv[i].clone());
                i += 1;
            }
        }
        Args { flags, rest }
    }
    fn get<T: std::str::FromStr>(&self, name: &str, default: T) -> T {
        match self.flags.iter().rev().find(|(n, _)| n == name) {
            None => default,
            Some((_, v)) => v
                .parse()
                .unwrap_or_else(|_| die(&format!("bad value for --{name}: {v}"))),
        }
    }
    fn str(&self, name: &str, default: &str) -> String {
        self.get(name, default.to_string())
    }
}

fn die(msg: &str) -> ! {
    eprintln!("error: {msg}\n\n{USAGE}");
    std::process::exit(2)
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = argv.first() else {
        print!("{USAGE}");
        return;
    };
    let args = Args::parse(&argv[1..]);
    match cmd.as_str() {
        "demo" => demo(&args),
        "bench" => bench(&args),
        "wiki" => wiki_cmd(&args),
        "-h" | "--help" | "help" => print!("{USAGE}"),
        other => die(&format!("unknown command {other}")),
    }
}

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

const STRATEGIES: [Strategy; 4] = [
    Strategy::Full,
    Strategy::Prune {
        slot_bits: 0,
        filter: false,
    },
    Strategy::Prune {
        slot_bits: 4,
        filter: false,
    },
    Strategy::Prune {
        slot_bits: 4,
        filter: true,
    },
];

/// Median (and minimum) of `reps` timings for one configuration, plus the stats and answer
/// of the last repetition.
struct Measured {
    label: String,
    total_ms: f64,
    total_min_ms: f64,
    merge_min_ms: f64,
    local_ms: f64,
    sketch_ms: f64,
    merge_ms: f64,
    stats: Stats,
    result: Vec<(u64, i64)>,
}

fn ms(d: std::time::Duration) -> f64 {
    d.as_secs_f64() * 1e3
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Run every config `reps` times, interleaved round-robin so slow drift
/// (thermal, background load) hits all of them equally, with the starting
/// config rotated each round so none of them always runs first. One warm-up
/// round.
fn measure(
    keys: &[u64],
    vals: &[i64],
    agg: AggKind,
    k: usize,
    cfgs: &[Config],
    reps: usize,
) -> Vec<Measured> {
    type Run = (Stats, Vec<(u64, i64)>);
    let mut runs: Vec<Vec<Run>> = cfgs.iter().map(|_| Vec::new()).collect();
    for round in 0..=reps {
        for j in 0..cfgs.len() {
            let i = (j + round) % cfgs.len();
            let (res, st) = top_k(keys, vals, agg, k, &cfgs[i]);
            if round > 0 {
                runs[i].push((st, res));
            }
        }
    }
    cfgs.iter()
        .zip(runs)
        .map(|(cfg, mut r)| {
            let pick = |f: fn(&Stats) -> std::time::Duration| {
                median(r.iter().map(|(s, _)| ms(f(s))).collect())
            };
            let total_ms = pick(|s| s.total);
            let local_ms = pick(|s| s.local);
            let sketch_ms = pick(|s| s.sketch);
            let merge_ms = pick(|s| s.merge);
            let lowest = |f: fn(&Stats) -> std::time::Duration| {
                r.iter()
                    .map(|(s, _)| ms(f(s)))
                    .fold(f64::INFINITY, f64::min)
            };
            let total_min_ms = lowest(|s| s.total);
            let merge_min_ms = lowest(|s| s.merge);
            let (stats, result) = r.pop().unwrap();
            Measured {
                label: cfg.strategy.label(),
                total_ms,
                total_min_ms,
                merge_min_ms,
                local_ms,
                sketch_ms,
                merge_ms,
                stats,
                result,
            }
        })
        .collect()
}

fn print_table(ms: &[Measured]) {
    let base = ms[0].total_ms;
    let base_merge = ms[0].merge_ms;
    println!(
        "  {:<22} {:>9} {:>9} {:>9} {:>9} {:>8} {:>8} {:>10} {:>9}",
        "strategy",
        "total ms",
        "local",
        "sketch",
        "merge",
        "speedup",
        "merge x",
        "partitions",
        "skipped"
    );
    for m in ms {
        println!(
            "  {:<22} {:>9.1} {:>9.1} {:>9.1} {:>9.1} {:>7.2}x {:>7.2}x {:>4}/{:<5} {:>8.1}%",
            m.label,
            m.total_ms,
            m.local_ms,
            m.sketch_ms,
            m.merge_ms,
            base / m.total_ms,
            base_merge / m.merge_ms,
            m.stats.partitions_merged,
            m.stats.partitions,
            100.0 * m.stats.entries_skipped_frac()
        );
    }
}

fn check_same(ms: &[Measured], expect: &[(u64, i64)]) {
    for m in ms {
        if m.result != expect {
            eprintln!("MISMATCH in {}: {:?} vs {:?}", m.label, m.result, expect);
            std::process::exit(1);
        }
    }
}

fn configs(threads: usize, partition_bits: u32, strategies: &[Strategy]) -> Vec<Config> {
    strategies
        .iter()
        .map(|&strategy| Config {
            threads,
            partition_bits,
            preagg_bits: 14,
            strategy,
        })
        .collect()
}

fn demo(a: &Args) {
    let rows: usize = a.get("rows", 20_000_000);
    let groups: usize = a.get("groups", 4_000_000);
    let skew: f64 = a.get("skew", 1.0);
    let k: usize = a.get("k", 10);
    let threads: usize = a.get("threads", default_threads());
    let reps: usize = a.get("reps", 5);

    let t = Instant::now();
    let (keys, vals) = zipf_rows(rows, groups, skew, 1, 100, 42, threads);
    println!(
        "{rows} rows, Zipf(s={skew}) over {groups} keys, generated in {:.0} ms; {threads} threads, 256 partitions",
        ms(t.elapsed())
    );
    for agg in [AggKind::Count, AggKind::Sum] {
        println!(
            "\nSELECT key, {} FROM t GROUP BY key ORDER BY 2 DESC LIMIT {k}",
            agg.name()
        );
        let t = Instant::now();
        let expect = reference::top_k(&keys, &vals, agg, k);
        let ref_ms = ms(t.elapsed());
        let m = measure(
            &keys,
            &vals,
            agg,
            k,
            &configs(threads, 8, &STRATEGIES),
            reps,
        );
        check_same(&m, &expect);
        print_table(&m);
        println!(
            "  {} distinct groups, {} thread-local entries to merge; all {} strategies match the reference \
             (HashMap + sort, {:.0} ms)",
            m[0].stats.groups_merged,
            m[0].stats.local_entries,
            m.len(),
            ref_ms
        );
        let top: Vec<String> = expect
            .iter()
            .take(3)
            .map(|(k, v)| format!("{k:016x}={v}"))
            .collect();
        println!("  top 3: {}", top.join("  "));
    }
}

// ---------------------------------------------------------------- bench ----

fn json_measured(out: &mut String, m: &Measured) {
    let s = &m.stats;
    let _ = write!(
        out,
        "{{\"strategy\":\"{}\",\"total_ms\":{:.3},\"total_min_ms\":{:.3},\"merge_min_ms\":{:.3},\"local_ms\":{:.3},\"sketch_ms\":{:.3},\"merge_ms\":{:.3},\
         \"partitions\":{},\"partitions_merged\":{},\"local_entries\":{},\"entries_merged\":{},\"entries_filtered\":{}}}",
        m.label,
        m.total_ms,
        m.total_min_ms,
        m.merge_min_ms,
        m.local_ms,
        m.sketch_ms,
        m.merge_ms,
        s.partitions,
        s.partitions_merged,
        s.local_entries,
        s.entries_merged,
        s.entries_filtered
    );
}

fn json_list(ms: &[Measured]) -> String {
    let mut s = String::from("[");
    for (i, m) in ms.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        json_measured(&mut s, m);
    }
    s.push(']');
    s
}

fn bench(a: &Args) {
    let rows: usize = a.get("rows", 20_000_000);
    let groups: usize = a.get("groups", 4_000_000);
    let reps: usize = a.get("reps", 5);
    let threads: usize = a.get("threads", default_threads());
    let out_path = a.str("out", "bench/results.json");
    let k = 10;
    let mut json = String::new();
    let _ = write!(
        json,
        "{{\"machine\":{{\"threads\":{threads},\"os\":\"{}\",\"arch\":\"{}\"}},\"rows\":{rows},\"groups\":{groups},\"reps\":{reps}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    // 1. Skew sweep: every strategy, COUNT(*), k = 10, 256 partitions.
    println!(
        "== skew sweep: {rows} rows over {groups} keys, COUNT(*), k={k}, {threads} threads =="
    );
    json.push_str(",\"skew\":[");
    for (i, s) in [0.0, 0.5, 0.8, 1.0, 1.2, 1.5].into_iter().enumerate() {
        let (keys, vals) = zipf_rows(rows, groups, s, 1, 100, 7, threads);
        let m = measure(
            &keys,
            &vals,
            AggKind::Count,
            k,
            &configs(threads, 8, &STRATEGIES),
            reps,
        );
        check_same(&m, &m[0].result.clone());
        println!("-- s = {s}");
        print_table(&m);
        let _ = write!(
            json,
            "{}{{\"s\":{s},\"runs\":{}}}",
            if i > 0 { "," } else { "" },
            json_list(&m)
        );
    }
    json.push(']');

    // 2. Slot ablation at s = 0.8, where partition totals alone prune nothing,
    //    with and without the in-partition filter.
    let (keys, vals) = zipf_rows(rows, groups, 0.8, 1, 100, 7, threads);
    println!("\n== slot ablation: s = 0.8, 256 partitions ==");
    let mut strategies = vec![Strategy::Full];
    for b in 0..=8 {
        strategies.push(Strategy::Prune {
            slot_bits: b,
            filter: false,
        });
        strategies.push(Strategy::Prune {
            slot_bits: b,
            filter: true,
        });
    }
    let m = measure(
        &keys,
        &vals,
        AggKind::Count,
        k,
        &configs(threads, 8, &strategies),
        reps,
    );
    check_same(&m, &m[0].result.clone());
    print_table(&m);
    let _ = write!(json, ",\"slots_s\":0.8,\"slots\":{}", json_list(&m));
    drop((keys, vals));
    let (keys, vals) = zipf_rows(rows, groups, 1.0, 1, 100, 7, threads);

    // 3. k sweep at s = 1.0.
    println!("\n== k sweep: s = 1.0 ==");
    json.push_str(",\"k\":[");
    for (i, kk) in [1usize, 10, 100, 1000, 10_000].into_iter().enumerate() {
        let m = measure(
            &keys,
            &vals,
            AggKind::Count,
            kk,
            &configs(threads, 8, &STRATEGIES),
            reps,
        );
        check_same(&m, &m[0].result.clone());
        println!("-- k = {kk}");
        print_table(&m);
        let _ = write!(
            json,
            "{}{{\"k\":{kk},\"runs\":{}}}",
            if i > 0 { "," } else { "" },
            json_list(&m)
        );
    }
    json.push(']');

    // 4. Partition count at s = 1.0.
    println!("\n== partition sweep: s = 1.0 ==");
    json.push_str(",\"partitions\":[");
    for (i, pb) in [4u32, 6, 8, 10, 12].into_iter().enumerate() {
        let m = measure(
            &keys,
            &vals,
            AggKind::Count,
            k,
            &configs(threads, pb, &STRATEGIES),
            reps,
        );
        check_same(&m, &m[0].result.clone());
        println!("-- {} partitions", 1 << pb);
        print_table(&m);
        let _ = write!(
            json,
            "{}{{\"partition_bits\":{pb},\"runs\":{}}}",
            if i > 0 { "," } else { "" },
            json_list(&m)
        );
    }
    json.push_str("]}\n");

    std::fs::write(&out_path, json).unwrap_or_else(|e| die(&format!("writing {out_path}: {e}")));
    println!("\nwrote {out_path}");
}

// ----------------------------------------------------------------- wiki ----

fn wiki_cmd(a: &Args) {
    let by = match a.str("by", "page").as_str() {
        "page" => GroupBy::Page,
        "title" => GroupBy::Title,
        o => die(&format!("--by must be page or title, not {o}")),
    };
    let agg = match a.str("agg", "sum").as_str() {
        "sum" => AggKind::Sum,
        "count" => AggKind::Count,
        "max" => AggKind::Max,
        o => die(&format!("--agg must be sum, count or max, not {o}")),
    };
    let k: usize = a.get("k", 10);
    let reps: usize = a.get("reps", 5);
    let threads: usize = a.get("threads", default_threads());
    let paths: Vec<PathBuf> = a.rest.iter().map(PathBuf::from).collect();
    if paths.is_empty() {
        die("wiki needs at least one pageviews file (run scripts/fetch-wiki.sh)");
    }

    let t = Instant::now();
    let (keys, vals) = wiki::load(&paths, by).unwrap_or_else(|e| die(&e.to_string()));
    println!(
        "loaded {} rows from {} file(s) in {:.1} s",
        keys.len(),
        paths.len(),
        t.elapsed().as_secs_f64()
    );
    let (col, what) = match by {
        GroupBy::Page => ("domain_code, page_title", "page"),
        GroupBy::Title => ("page_title", "title"),
    };
    let agg_sql = match agg {
        AggKind::Count => "COUNT(*)",
        AggKind::Sum => "SUM(count_views)",
        AggKind::Max => "MAX(count_views)",
    };
    println!("\nSELECT {col}, {agg_sql} FROM pageviews GROUP BY {col} ORDER BY 2 DESC LIMIT {k}");

    let m = measure(
        &keys,
        &vals,
        agg,
        k,
        &configs(threads, 8, &STRATEGIES),
        reps,
    );
    check_same(&m, &m[0].result.clone());

    let wanted: Vec<u64> = m[0].result.iter().map(|r| r.0).collect();
    let names = wiki::names(&paths, by, &wanted).unwrap_or_else(|e| die(&e.to_string()));
    println!();
    for (i, (key, val)) in m[0].result.iter().enumerate() {
        let n = names.get(key).cloned().unwrap_or_default();
        let shown = n.first().cloned().unwrap_or_else(|| format!("{key:016x}"));
        let flag = if n.len() > 1 {
            "  (HASH COLLISION: also ".to_string() + &n[1..].join(", ") + ")"
        } else {
            String::new()
        };
        println!("  {:>2}. {:>10}  {}{}", i + 1, val, shown, flag);
    }
    println!();
    print_table(&m);
    let full = &m[0].stats;
    println!(
        "  {} distinct {what}s, {} thread-local entries across {} partitions; answers identical across strategies",
        full.groups_merged, full.local_entries, full.partitions
    );

    if let Some((_, path)) = a.flags.iter().find(|(n, _)| n == "json") {
        let mut j = String::new();
        let _ = write!(j, "{{\"rows\":{},\"files\":{},\"by\":\"{what}\",\"agg\":\"{}\",\"k\":{k},\"threads\":{threads},\"runs\":{}", keys.len(), paths.len(), agg.name(), json_list(&m));
        // Per-partition detail of every strategy's last run, for the partition map:
        // [bound, partials, merged, filtered].
        j.push_str(",\"detail\":[");
        for (ri, run) in m.iter().enumerate() {
            j.push_str(if ri > 0 { ",[" } else { "[" });
            for (i, d) in run.stats.detail.iter().enumerate() {
                let _ = write!(
                    j,
                    "{}[{},{},{},{}]",
                    if i > 0 { "," } else { "" },
                    d.bound,
                    d.entries,
                    d.merged as u8,
                    d.filtered
                );
            }
            j.push(']');
        }
        j.push(']');
        j.push_str("}\n");
        std::fs::write(path, j).unwrap_or_else(|e| die(&format!("writing {path}: {e}")));
        println!("wrote {path}");
    }
}
