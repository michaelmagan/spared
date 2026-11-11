#!/usr/bin/env python3
"""Pass E summary: median and range of each metric per variant, the difference of medians, paired per-round differences, and a noise verdict.
Verdict: 'within noise' when the medians differ by less than the larger within-variant range, or the paired differences do not all share a sign."""
import glob, json, re, statistics as st, sys, os
R = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "results")
def load(v, piece):
    out = {}
    for f in glob.glob(f"{R}/e-passE-{v}-{piece}-run*.json"):
        r = int(re.search(r"run(\d+)\.json", f).group(1))
        out[r] = json.load(open(f))
    return out
def rows():
    for piece in ["warm-c1", "warm-c16", "mixed-c1", "mixed-c16"]:
        d = {v: load(v, piece) for v in ("before", "after")}
        for name, fn in [("p50 us", lambda j: j["full_body_us"]["p50"]), ("p99 us", lambda j: j["full_body_us"]["p99"]),
                         ("req/s", lambda j: j["requests"] / j["wall_s"])]:
            yield f"{piece} {name}", d, fn, name != "req/s"
    for piece in ["noop", "sql-agg", "sql-write", "file-write", "file-small", "post-1kb", "post-1mb"]:
        d = {v: load(v, piece) for v in ("before", "after")}
        yield f"{piece} p50 us", d, lambda j: j["full_body_us"]["p50"], True
        yield f"{piece} p99 us", d, lambda j: j["full_body_us"]["p99"], True
        yield f"{piece} server cpu us/req", d, lambda j: j["server_cpu_us_per_request"], True
def main():
    print("| metric | before median (min-max) | after median (min-max) | delta | paired rounds worse/better | verdict |")
    print("|---|---|---|---|---|---|")
    for label, d, fn, lower_better in rows():
        vals = {v: {r: fn(j) for r, j in d[v].items()} for v in d}
        if not vals["before"] or not vals["after"]:
            continue
        b, a = list(vals["before"].values()), list(vals["after"].values())
        mb, ma = st.median(b), st.median(a)
        delta = (ma - mb) / mb * 100
        rounds = sorted(set(vals["before"]) & set(vals["after"]))
        diffs = [(vals["after"][r] - vals["before"][r]) * (1 if lower_better else -1) for r in rounds]
        worse, better = sum(x > 0 for x in diffs), sum(x < 0 for x in diffs)
        spread = max(max(b) - min(b), max(a) - min(a))
        noise = abs(ma - mb) < spread or not (worse == len(diffs) or better == len(diffs))
        verdict = "within noise" if noise else ("REGRESSION" if worse == len(diffs) else "improvement")
        f = lambda x: f"{x:.1f}" if x < 1000 else f"{x:.0f}"
        print(f"| {label} | {f(mb)} ({f(min(b))}-{f(max(b))}) | {f(ma)} ({f(min(a))}-{f(max(a))}) | {delta:+.1f}% | {worse}/{better} of {len(diffs)} | {verdict} |")
    steal = {}
    for line in open(f"{R}/e-passE-steal.jsonl"):
        j = json.loads(line); m = re.match(r"r\d+-(\w)-(\w+)", j["segment"])
        steal.setdefault((m.group(2), m.group(1)), []).append(j["steal_pct"])
    print("\nSteal % per segment (median, max): " + "; ".join(f"{v}/{p}: {st.median(x):.2f}, {max(x):.2f}" for (v, p), x in sorted(steal.items())))
main()
