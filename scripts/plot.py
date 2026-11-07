#!/usr/bin/env python3
"""Render the README charts as SVG from benchmark JSON. Standard library only.

    python3 scripts/plot.py                       # GitHub runner results (default)
    python3 scripts/plot.py bench/wiki-m2.json bench/results-m2.json

Writes assets/wiki.svg, assets/partition-map.svg, assets/skew.svg and
assets/slots.svg. Hover any mark for its exact value (SVG <title>).
"""

import json
import math
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ASSETS = ROOT / "assets"

# Colors: three categorical hues checked for color-vision-deficiency
# separation on this light background, plus neutral grays for chrome.
SURFACE = "#fcfcfb"
INK = "#0b0b0b"
INK2 = "#52514e"
MUTED = "#898781"
GRID = "#e1e0d9"
AXIS = "#c3c2b7"
S1, S2, S3 = "#2a78d6", "#eb6834", "#1baf7a"
FONT = 'system-ui, -apple-system, "Segoe UI", sans-serif'

MACHINE = {"macos": "Apple M2 (macOS)", "linux": "GitHub Actions runner (Linux)"}

STRATEGY_COLOR = {
    "slots (16) + filter": S1,
    "slots (16)": S2,
    "partition totals": S3,
}


def esc(s):
    return str(s).replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


class Svg:
    def __init__(self, w, h):
        self.w, self.h, self.parts = w, h, []

    def add(self, s):
        self.parts.append(s)

    def text(self, x, y, s, size=13, fill=INK2, anchor="start", weight=400, extra=""):
        self.add(
            f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" fill="{fill}" '
            f'text-anchor="{anchor}" font-weight="{weight}" {extra}>{esc(s)}</text>'
        )

    def line(self, x1, y1, x2, y2, stroke=GRID, width=1):
        self.add(f'<line x1="{x1:.1f}" y1="{y1:.1f}" x2="{x2:.1f}" y2="{y2:.1f}" stroke="{stroke}" stroke-width="{width}"/>')

    def rect(self, x, y, w, h, fill, rx=0, title=None):
        t = f"<title>{esc(title)}</title>" if title else ""
        self.add(f'<rect x="{x:.1f}" y="{y:.1f}" width="{max(w, 0):.1f}" height="{h:.1f}" rx="{rx}" fill="{fill}">{t}</rect>')

    def save(self, path):
        body = "\n".join(self.parts)
        svg = (
            f'<svg xmlns="http://www.w3.org/2000/svg" width="{self.w}" height="{self.h}" '
            f'viewBox="0 0 {self.w} {self.h}" font-family=\'{FONT}\'>\n'
            f'<rect width="100%" height="100%" rx="8" fill="{SURFACE}"/>\n{body}\n</svg>\n'
        )
        path.write_text(svg)
        print("wrote", path.relative_to(ROOT))


def bar_end(svg, x, y, w, h, fill, title):
    """A horizontal bar segment with a 4px rounded data end (right side only)."""
    r = min(4, w / 2, h / 2)
    if w <= 0:
        return
    svg.add(
        f'<path d="M{x:.1f},{y:.1f} h{w - r:.1f} a{r},{r} 0 0 1 {r},{r} v{h - 2 * r:.1f} '
        f'a{r},{r} 0 0 1 -{r},{r} h-{w - r:.1f} z" fill="{fill}"><title>{esc(title)}</title></path>'
    )


def wiki_chart(wiki):
    runs = wiki["runs"]
    W, H = 900, 360
    left, right, top = 170, 270, 92
    row_h, gap = 34, 18
    svg = Svg(W, H)
    hours = wiki["files"]
    svg.text(24, 34, f"Top 10 of {wiki['rows'] / 1e6:.1f}M Wikipedia pageview rows: who skips the merge?", 17, INK, weight=600)
    machine = MACHINE.get(wiki.get("os"), wiki.get("os", "?"))
    svg.text(
        24, 58,
        f"SUM(views) GROUP BY (wiki, page) ORDER BY 2 DESC LIMIT {wiki['k']}: {hours} hourly dumps, "
        f"{wiki['threads']} threads, {machine}, median of {wiki.get('reps', 7)} runs.",
        12.5, INK2,
    )
    maxv = max(r["total_ms"] for r in runs)
    scale = (W - left - right) / maxv
    # ticks
    step = 500 if maxv > 1500 else 100
    t = 0
    while t <= maxv * 1.001:
        x = left + t * scale
        svg.line(x, top - 8, x, top + len(runs) * (row_h + gap) - gap + 4, GRID)
        svg.text(x, top + len(runs) * (row_h + gap) + 12, f"{t / 1000:.1f} s" if step >= 500 else f"{t:.0f} ms", 11, MUTED, "middle")
        t += step
    base = runs[0]["total_ms"]
    for i, r in enumerate(runs):
        y = top + i * (row_h + gap)
        local = r["local_ms"]
        merge = r["merge_ms"]
        other = max(r["total_ms"] - local - merge, 0)
        svg.text(left - 12, y + row_h / 2 + 5, r["strategy"], 13, INK, "end", 500)
        x = left
        svg.rect(x, y, local * scale - 1, row_h, "#c3c2b7", 0, f"{r['strategy']}: local phase {local:.0f} ms")
        x += local * scale + 1
        if other > 0:
            svg.rect(x, y, max(other * scale - 1, 0), row_h, "#e1e0d9", 0, f"sketch + bookkeeping {other:.0f} ms")
            x += other * scale + 1
        bar_end(svg, x, y, max(merge * scale, 2), row_h, S1, f"{r['strategy']}: merge phase {merge:.0f} ms")
        x_end = left + r["total_ms"] * scale + 12
        speed = base / r["total_ms"]
        svg.text(x_end, y + 14, f"{r['total_ms'] / 1000:.2f} s" + ("" if i == 0 else f"  ({speed:.2f}x)"), 13, INK, weight=600)
        svg.text(
            x_end, y + 30,
            f"merged {r['partitions_merged']}/{r['partitions']} partitions, merge {merge:.0f} ms",
            11.5, INK2,
        )
    # legend
    ly = H - 22
    lx = 24
    for label, color in [("local phase (pre-aggregate + flush)", "#c3c2b7"), ("merge phase", S1), ("other", "#e1e0d9")]:
        svg.rect(lx, ly - 10, 12, 12, color, 2)
        svg.text(lx + 18, ly, label, 12, INK2)
        lx += 18 + len(label) * 6.6 + 22
    svg.save(ASSETS / "wiki.svg")


def partition_map(wiki):
    runs = wiki["runs"]
    details = wiki["detail"]
    pick = [1, len(runs) - 1]  # partition totals vs slots + filter
    n = len(details[0])
    side = int(round(math.sqrt(n)))
    cell, pad = 14, 2
    grid_w = side * (cell + pad)
    W = 2 * grid_w + 150
    H = grid_w + 150
    svg = Svg(W, H)
    svg.text(24, 32, "Which of the 256 partitions had to be merged (Wikipedia, 6 h)", 16, INK, weight=600)
    svg.text(24, 54, "One square per partition, in hash order. Hover for its sketch bound and partial count.", 12.5, INK2)
    for gi, ri in enumerate(pick):
        ox = 24 + gi * (grid_w + 100)
        oy = 92
        r = runs[ri]
        svg.text(ox, oy - 14, f"{r['strategy']}: {r['partitions_merged']} merged", 13, INK, weight=600)
        for p, (bound, entries, merged, filtered) in enumerate(details[ri]):
            x = ox + (p % side) * (cell + pad)
            y = oy + (p // side) * (cell + pad)
            fill = S1 if merged else GRID
            state = "merged" if merged else "pruned"
            extra = f", {filtered:,} partials filtered" if filtered else ""
            svg.rect(x, y, cell, cell, fill, 2, f"partition {p}: {state}, bound {bound:,}, {entries:,} partials{extra}")
    ly = H - 24
    svg.rect(24, ly - 10, 12, 12, S1, 2)
    svg.text(42, ly, "merged", 12, INK2)
    svg.rect(110, ly - 10, 12, 12, GRID, 2)
    svg.text(128, ly, "skipped without reading its partials", 12, INK2)
    svg.save(ASSETS / "partition-map.svg")


def line_panel(svg, ox, oy, pw, ph, xs, series, ytitle, ymax, xlabel, xfmt=str, log2x=False, yoff=0):
    """One panel of a small-multiple line chart. series: [(label, color, ys)]."""
    def X(v):
        if log2x:
            lo, hi = math.log2(xs[0]), math.log2(xs[-1])
            return ox + (math.log2(v) - lo) / (hi - lo) * pw
        return ox + (v - xs[0]) / (xs[-1] - xs[0]) * pw

    def Y(v):
        return oy + ph - v / ymax * ph

    svg.text(ox, oy - 14, ytitle, 13, INK, weight=600)
    nt = 4 if ymax in (256, 1024, 4096) else 5
    for i in range(nt + 1):
        v = ymax * i / nt
        svg.line(ox, Y(v), ox + pw, Y(v), GRID)
        svg.text(ox - 8, Y(v) + 4, f"{v + yoff:g}", 11, MUTED, "end")
    svg.line(ox, oy + ph, ox + pw, oy + ph, AXIS)
    for v in xs:
        svg.text(X(v), oy + ph + 16, xfmt(v), 11, MUTED, "middle")
    svg.text(ox + pw / 2, oy + ph + 34, xlabel, 11.5, INK2, "middle")
    for label, color, ys in series:
        pts = " ".join(f"{X(x):.1f},{Y(y):.1f}" for x, y in zip(xs, ys))
        svg.add(f'<polyline points="{pts}" fill="none" stroke="{color}" stroke-width="2" stroke-linejoin="round"/>')
        for x, y in zip(xs, ys):
            svg.add(
                f'<circle cx="{X(x):.1f}" cy="{Y(y):.1f}" r="4" fill="{color}" stroke="{SURFACE}" stroke-width="2">'
                f"<title>{esc(label)} at {xfmt(x)}: {y + yoff:.2f}</title></circle>"
            )
    return X, Y


def skew_chart(res):
    sweep = res["skew"]
    xs = [p["s"] for p in sweep]
    names = [r["strategy"] for r in sweep[0]["runs"]][1:]
    W, H = 1000, 420
    svg = Svg(W, H)
    svg.text(24, 32, "What gets pruned, and what it buys, by key skew (synthetic, COUNT(*), k = 10)", 16, INK, weight=600)
    svg.text(
        24, 54,
        f"{res['rows'] / 1e6:.0f}M rows over {res['groups'] / 1e6:.0f}M keys, Zipf exponent s (0 = uniform), "
        f"{res['machine']['threads']} threads, {MACHINE.get(res['machine']['os'], res['machine']['os'])}, median of {res['reps']}. Right: gray line = full merge.",
        12.5, INK2,
    )
    pw, ph = 330, 230
    total_parts = sweep[0]["runs"][0]["partitions"]
    panels = [(f"partitions merged (of {total_parts})", "partitions_merged"), ("end-to-end speedup (x)", "total_ms")]
    for pi, (title, field) in enumerate(panels):
        series = []
        for name in names:
            ys = []
            for p in sweep:
                r = next(r for r in p["runs"] if r["strategy"] == name)
                ys.append(r[field] if field == "partitions_merged" else p["runs"][0][field] / r[field])
            series.append((name, STRATEGY_COLOR[name], ys))
        if field == "partitions_merged":
            ymax = total_parts
        else:
            ymax = math.ceil(max(max(s[2]) for s in series) / 0.5) * 0.5
        ox = 70 + pi * (pw + 230)
        X, Y = line_panel(svg, ox, 100, pw, ph, xs, series, title, ymax, "Zipf exponent s")
        if field != "partitions_merged":
            svg.line(ox, Y(1), ox + pw, Y(1), AXIS, 1.5)
    lx = 70
    for name in names:
        svg.rect(lx, H - 30, 12, 12, STRATEGY_COLOR[name], 2)
        svg.text(lx + 18, H - 20, name, 12, INK2)
        lx += 18 + len(name) * 6.8 + 26
    svg.save(ASSETS / "skew.svg")


def slots_chart(res):
    runs = res["slots"]
    base = runs[0]
    pts = {}
    for r in runs[1:]:
        lab = r["strategy"]
        filt = lab.endswith("+ filter")
        if lab == "partition totals":
            n = 1
        elif lab == "partition totals + filter":
            n = 1
        else:
            n = int(lab.split("(")[1].split(")")[0])
        pts.setdefault(filt, {})[n] = r
    xs = sorted(pts[False])
    W, H = 900, 420
    svg = Svg(W, H)
    svg.text(24, 32, f"More slots per partition, tighter bounds (synthetic, Zipf s = {res.get('slots_s', 1.0)})", 16, INK, weight=600)
    svg.text(24, 54, f"1 slot = the partition total. {MACHINE.get(res['machine']['os'], res['machine']['os'])}. Left: partitions still merged. Right: partials never inserted.", 12.5, INK2)
    pw, ph = 330, 230
    total = base["partitions"]
    s_merged = [("slots", S2, [pts[False][x]["partitions_merged"] for x in xs]), ("slots + filter", S1, [pts[True][x]["partitions_merged"] for x in xs])]
    ymax = total
    line_panel(svg, 70, 100, pw, ph, xs, s_merged, f"partitions merged (of {total})", ymax, "slots per partition", str, True)
    s_skip = [
        ("slots", S2, [100 * (1 - pts[False][x]["entries_merged"] / pts[False][x]["local_entries"]) for x in xs]),
        ("slots + filter", S1, [100 * (1 - pts[True][x]["entries_merged"] / pts[True][x]["local_entries"]) for x in xs]),
    ]
    lo = math.floor(min(min(s[2]) for s in s_skip) / 5) * 5
    # Re-base the right panel at `lo` so the differences are visible; say so.
    shifted = [(n, c, [v - lo for v in ys]) for n, c, ys in s_skip]
    X, Y = line_panel(svg, 510, 100, pw, ph, xs, shifted, (f"partials skipped (%, axis starts at {lo})" if lo else "partials skipped (%)"), 100 - lo, "slots per partition", str, True, lo)
    lx = 70
    for name, color in [("slots", S2), ("slots + filter", S1)]:
        svg.rect(lx, H - 30, 12, 12, color, 2)
        svg.text(lx + 18, H - 20, name, 12, INK2)
        lx += 18 + len(name) * 6.8 + 26
    svg.save(ASSETS / "slots.svg")


def main():
    ASSETS.mkdir(exist_ok=True)
    args = sys.argv[1:] or ["bench/wiki-gha.json", "bench/results-gha.json"]
    for a in args:
        d = json.loads((ROOT / a).read_text() if not Path(a).is_absolute() else Path(a).read_text())
        if "detail" in d:
            wiki_chart(d)
            partition_map(d)
        if "skew" in d:
            skew_chart(d)
            slots_chart(d)


if __name__ == "__main__":
    main()
