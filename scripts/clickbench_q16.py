#!/usr/bin/env python3
"""ClickBench Q16, the query the Pivot post measured, on a slice of `hits`.

    uv run --with duckdb --with numpy python scripts/clickbench_q16.py --files 10

Q16: SELECT UserID, SearchPhrase, COUNT(*) FROM hits
     GROUP BY UserID, SearchPhrase ORDER BY COUNT(*) DESC LIMIT 10

Downloads the first N of ClickBench's 100 partitioned parquet files (~120 MB
and ~1M rows each) into data/clickbench/. It then:

1. times Q16 in DuckDB on an in-memory table (median of 7 after a warm-up),
   both as written and grouped by a precomputed 64-bit hash of
   (UserID, SearchPhrase);
2. writes that hash column as raw little-endian u64 to data/q16.u64, so
   `groupcut keys --check data/q16.u64` answers the same query.

The counts of the two top 10s can be compared directly. Keys differ, since
DuckDB prints the columns and groupcut prints the hash.
"""

import argparse
import statistics
import time
import subprocess
from pathlib import Path

import duckdb

ROOT = Path(__file__).resolve().parent.parent
URL = "https://datasets.clickhouse.com/hits_compatible/athena_partitioned/hits_{}.parquet"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--files", type=int, default=10)
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--reps", type=int, default=7)
    ap.add_argument("--out", default=str(ROOT / "data" / "q16.u64"))
    a = ap.parse_args()

    d = ROOT / "data" / "clickbench"
    d.mkdir(parents=True, exist_ok=True)
    files = []
    for i in range(a.files):
        f = d / f"hits_{i}.parquet"
        if not f.exists() or f.stat().st_size == 0:
            print(f"fetch {f.name}", flush=True)
            tmp = f.with_suffix(".part")
            subprocess.run(["curl", "-fsSL", "--retry", "3", "-o", str(tmp), URL.format(i)], check=True)
            tmp.rename(f)
        files.append(str(f))

    con = duckdb.connect()
    con.execute(f"SET threads = {a.threads}")
    con.execute("CREATE TABLE hits AS SELECT UserID, SearchPhrase FROM read_parquet(?)", [files])
    con.execute("CREATE TABLE hk AS SELECT hash(UserID, SearchPhrase)::UBIGINT AS k FROM hits")
    rows = con.execute("SELECT count(*) FROM hits").fetchone()[0]
    print(f"duckdb {duckdb.__version__}: {rows} rows from {len(files)} files, threads={a.threads}")

    queries = {
        "Q16 as written": "SELECT UserID, SearchPhrase, COUNT(*) AS c FROM hits "
        "GROUP BY UserID, SearchPhrase ORDER BY c DESC LIMIT 10",
        "Q16 on hashed key": "SELECT k, COUNT(*) AS c FROM hk GROUP BY k ORDER BY c DESC LIMIT 10",
    }
    for name, q in queries.items():
        con.execute(q).fetchall()
        times = []
        for _ in range(a.reps):
            t = time.perf_counter()
            res = con.execute(q).fetchall()
            times.append((time.perf_counter() - t) * 1e3)
        print(f"  {name:<18} median {statistics.median(times):7.1f} ms   best {min(times):7.1f} ms")
        print("     counts: " + " ".join(str(r[-1]) for r in res))

    keys = con.execute("SELECT k FROM hk").fetchnumpy()["k"]
    keys.astype("<u8").tofile(a.out)
    print(f"wrote {len(keys)} keys to {a.out}")


if __name__ == "__main__":
    main()
