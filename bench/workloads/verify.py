#!/usr/bin/env python3
"""Call every workload route on both servers, plain and with ?verify=1, and compare the JSON results.

Usage: verify.py [--spared http://127.0.0.1:8180] [--spared-host workloads.localhost] [--workerd http://127.0.0.1:9095] [--out FILE]

Every field must match exactly, except where the two sides use different libraries that are allowed to produce different
bytes: markdown (html_len, html_sha256; the source document and the tag counts must match) and gzip (out_len, gzip_b64;
both outputs are decompressed here and must equal each other byte for byte). Exit code 1 on any mismatch.
"""
import argparse
import base64
import gzip
import hashlib
import json
import sys
import urllib.request

WORKLOADS = ["noop", "sha256", "json", "sql-agg", "sql-write", "markdown", "sieve", "nbody", "pixels", "sort",
             "wordfreq", "html", "gzip", "regex", "pbkdf2"]
ALLOWED_DIFF = {"markdown": {"html_len", "html_sha256"}, "gzip": {"out_len", "gzip_b64"}}


def get(base, path, host=None):
    req = urllib.request.Request(base + path, headers={"Host": host} if host else {})
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.loads(r.read())


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--spared", default="http://127.0.0.1:8180")
    ap.add_argument("--spared-host", default="workloads.localhost")
    ap.add_argument("--workerd", default="http://127.0.0.1:9095")
    ap.add_argument("--out")
    a = ap.parse_args()
    s = lambda p: get(a.spared, p, a.spared_host)
    w = lambda p: get(a.workerd, p)
    report = {"seed": {"spared": s("/w/sql-seed"), "workerd": w("/w/sql-seed")}, "workloads": {}}
    bad = 0
    for name in WORKLOADS:
        entry = {}
        for q in ("", "?verify=1"):
            rs, rw = s(f"/w/{name}{q}"), w(f"/w/{name}{q}")
            keys = sorted(set(rs) | set(rw))
            diff = [k for k in keys if rs.get(k) != rw.get(k)]
            unexpected = [k for k in diff if k not in ALLOWED_DIFF.get(name, set())]
            entry["verify" if q else "plain"] = {"spared": rs, "workerd": rw, "differs": diff, "unexpected": unexpected}
            if unexpected:
                bad += 1
            if name == "gzip" and q:
                ds = gzip.decompress(base64.b64decode(rs["gzip_b64"]))
                dw = gzip.decompress(base64.b64decode(rw["gzip_b64"]))
                same = ds == dw and len(ds) == rs["in_len"]
                entry["decompressed_equal"] = same
                entry["decompressed_sha256"] = hashlib.sha256(ds).hexdigest()
                for side in ("spared", "workerd"):
                    entry["verify"][side]["gzip_b64"] = f"<{len(entry['verify'][side]['gzip_b64'])} base64 chars>"
                if not same:
                    bad += 1
        status = "MATCH" if not entry["plain"]["unexpected"] and not entry["verify"]["unexpected"] else "MISMATCH"
        if name == "gzip" and not entry["decompressed_equal"]:
            status = "MISMATCH"
        allowed = sorted(set(entry["plain"]["differs"]) | set(entry["verify"]["differs"]))
        entry["status"] = status
        report["workloads"][name] = entry
        note = f" (allowed to differ: {', '.join(allowed)})" if allowed and status == "MATCH" else ""
        print(f"{name:10s} {status}{note}")
        if status != "MATCH":
            for q in ("plain", "verify"):
                for k in entry[q]["unexpected"]:
                    print(f"    {q} {k}: spared={str(entry[q]['spared'].get(k))[:120]} workerd={str(entry[q]['workerd'].get(k))[:120]}")
    report["mismatches"] = bad
    if a.out:
        with open(a.out, "w") as f:
            json.dump(report, f, indent=1)
    print("all outputs match" if bad == 0 else f"{bad} mismatches")
    sys.exit(1 if bad else 0)


if __name__ == "__main__":
    main()
