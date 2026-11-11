#!/usr/bin/env python3
"""Turn the batch files written by bench.py and mem.py into one file per lever:
bench/results/sqlite-tuning-<lever>.json. usage: collect.py <dir with batch json> <out dir>"""
import glob, json, os, re, sys

src, out = sys.argv[1], sys.argv[2]
levers = {}


def lever(name):
    return levers.setdefault(name, {"lever": name, "timing": [], "memory": []})


for f in sorted(glob.glob(os.path.join(src, "final-*.json"))):
    d = json.load(open(f))
    batch = os.path.basename(f)[:-5]
    base = d["variants"].get("base", {}).get("geomean")
    for label, v in d["variants"].items():
        if v["kind"] != "wasm":
            continue
        nat = d["variants"][v["vs"]]
        m = {
            "batch": batch, "journal_mode": d["journal"], "cpus": d["cpus"], "rows": d["n"], "reps": d["reps"],
            "statistic": d["stat"], "loadavg_limit_before_each_run": d["quiet_limit"],
            "engine_knobs": v["tune"], "extra_sql": v["extra_sql"], "native_comparator": v["vs"],
            "native_binary": nat["binary"], "wasm_binary": v["binary"],
            "ratio": v["ratio"], "geomean": v["geomean"], "wasm_us": v["us"], "native_us": nat["us"],
            "baseline_geomean_same_batch": base, "wasm_runs": v["runs"], "native_runs": nat["runs"],
        }
        L = lever(label)
        L["timing"].append(m)
        L.setdefault("wasm_bytes", v.get("wasm_bytes"))
        L.setdefault("cwasm_bytes", v.get("cwasm_bytes"))

for f in sorted(glob.glob(os.path.join(src, "mem*.json"))):
    d = json.load(open(f))
    name = re.sub(r"-rep\d+$", "", d["label"])
    lever("mem-" + name)["memory"].append({
        "file": os.path.basename(f), "apps_hot": d["apps_hot"], "engine_knobs": d["tune"], "distinct_sha256": d["unique_sha256"],
        "wasm_bytes": d["wasm_bytes_median"], "cwasm_bytes": d["cwasm_bytes_median"],
        "kb_per_hot_app_growth": d["kb_per_hot_app_growth"], "kb_total": d["kb_total"],
    })

os.makedirs(out, exist_ok=True)
for name, L in sorted(levers.items()):
    json.dump(L, open(os.path.join(out, f"sqlite-tuning-{name}.json"), "w"), indent=1)
print(len(levers), "lever files written to", out)
