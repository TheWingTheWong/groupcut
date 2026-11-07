#!/usr/bin/env python3
"""Run the README's Wikipedia query in DuckDB, for scale.

    uv run --with duckdb python scripts/duckdb_compare.py --threads 4 data/*.gz

Loads the dumps into an in-memory table (not timed), then times two queries,
median of 7 after one warm-up:

  string keys  GROUP BY domain_code, page_title   (the real query)
  hashed keys  GROUP BY hash(domain_code, page_title) as one BIGINT column,
               which is the input groupcut sees

and prints DuckDB's top 10 for comparison with groupcut's (bench/wiki-*.txt);
it does not compare them automatically.
"""

import argparse
import statistics
import time

import duckdb


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--reps", type=int, default=7)
    ap.add_argument("files", nargs="+")
    a = ap.parse_args()

    con = duckdb.connect()
    con.execute(f"SET threads = {a.threads}")
    t = time.perf_counter()
    con.execute(
        """
        CREATE TABLE pv AS
        SELECT domain_code, page_title, views
        FROM read_csv(?, delim = ' ', header = false, quote = '', escape = '',
                      columns = {'domain_code': 'VARCHAR', 'page_title': 'VARCHAR',
                                 'views': 'BIGINT', 'bytes': 'BIGINT'},
                      ignore_errors = true)
        """,
        [a.files],
    )
    con.execute("CREATE TABLE pvh AS SELECT hash(domain_code, page_title)::UBIGINT AS k, views FROM pv")
    rows = con.execute("SELECT count(*) FROM pv").fetchone()[0]
    print(f"duckdb {duckdb.__version__}: loaded {rows} rows in {time.perf_counter() - t:.1f} s, threads={a.threads}")

    queries = {
        "string keys": "SELECT domain_code, page_title, sum(views) AS s FROM pv "
        "GROUP BY domain_code, page_title ORDER BY s DESC, domain_code, page_title LIMIT 10",
        "hashed keys": "SELECT k, sum(views) AS s FROM pvh GROUP BY k ORDER BY s DESC, k LIMIT 10",
    }
    for name, q in queries.items():
        con.execute(q).fetchall()  # warm-up
        times = []
        for _ in range(a.reps):
            t = time.perf_counter()
            res = con.execute(q).fetchall()
            times.append((time.perf_counter() - t) * 1e3)
        print(f"  {name:<12} median {statistics.median(times):7.1f} ms   best {min(times):7.1f} ms   ({a.reps} runs)")
        if name == "string keys":
            for i, r in enumerate(res, 1):
                print(f"  {i:>4}. {r[2]:>10}  {r[0]} {r[1]}")


if __name__ == "__main__":
    main()
