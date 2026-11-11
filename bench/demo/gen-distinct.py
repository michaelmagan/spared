#!/usr/bin/env python3
"""Generate N genuinely different variants of apps/demo-spared under bench/work/distinct/vK (not committed).
Variant k has its own page text, a k-dependent number of routes, and per-route handlers with different constants,
loops and SQL, so the compiled code differs in content and size and the optimizer cannot merge variants.
--lean writes the same variants under bench/work/distinct-lean with a size-optimized profile (opt-level "s", panic abort); build them
with LEAN_SQLITE below in the environment so SQLite is compiled -Os without the optional modules rusqlite enables (FTS3/5, R-tree, JSON, ...).
--native writes the same variants of apps/demo-native (the native Rust baseline) under bench/work/distinct-native; build them for the host
target with `cargo build --release`.
Usage: gen-distinct.py [N] [--lean | --native]"""
import os, random, shutil, sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
NATIVE = "--native" in sys.argv
SRC = os.path.join(ROOT, "apps", "demo-native" if NATIVE else "demo-spared")
LEAN = "--lean" in sys.argv
ARGS = [a for a in sys.argv[1:] if a not in ("--lean", "--native")]
OUT = os.path.join(ROOT, "bench", "work", "distinct-lean" if LEAN else "distinct-native" if NATIVE else "distinct")
CRATE = "demo-native" if NATIVE else "demo-spared"
N = int(ARGS[0]) if ARGS else 50
LEAN_SQLITE = ("-USQLITE_ENABLE_FTS3 -USQLITE_ENABLE_FTS3_PARENTHESIS -USQLITE_ENABLE_FTS5 -USQLITE_ENABLE_RTREE -USQLITE_ENABLE_DBSTAT_VTAB "
               "-USQLITE_ENABLE_STAT4 -USQLITE_SOUNDEX -USQLITE_ENABLE_COLUMN_METADATA -USQLITE_ENABLE_JSON1 -DSQLITE_OMIT_JSON "
               "-DSQLITE_OMIT_DEPRECATED -DSQLITE_OMIT_SHARED_CACHE -DSQLITE_DEFAULT_MEMSTATUS=0 -DSQLITE_DQS=0")
WORDS = "alpha bravo cedar delta ember fjord grove harbor indigo juniper kestrel lantern meadow nectar orchid pebble quartz ridge summit thistle umber violet willow xenon yarrow zephyr".split()

main = open(os.path.join(SRC, "src", "main.rs")).read()
page = open(os.path.join(ROOT, "apps", "demo-shared", "page.html")).read()
os.makedirs(os.path.join(OUT, "demo-shared"), exist_ok=True)

for k in range(N):
    rnd = random.Random(1000 + k)
    d = os.path.join(OUT, f"v{k}")
    os.makedirs(os.path.join(d, "src"), exist_ok=True)
    cargo = open(os.path.join(SRC, "Cargo.toml")).read().replace(f'name = "{CRATE}"', f'name = "demo-distinct-{k}"')
    if LEAN:
        assert "opt-level = 3" in cargo
        cargo = cargo.replace("opt-level = 3", 'opt-level = "s"\npanic = "abort"')
    open(os.path.join(d, "Cargo.toml"), "w").write(cargo if "[workspace]" in cargo else cargo + "\n[workspace]\n")
    shutil.copy(os.path.join(SRC, "Cargo.lock"), os.path.join(d, "Cargo.lock"))
    lock = open(os.path.join(d, "Cargo.lock")).read().replace(f'name = "{CRATE}"', f'name = "demo-distinct-{k}"')
    open(os.path.join(d, "Cargo.lock"), "w").write(lock)
    title = " ".join(rnd.choice(WORDS) for _ in range(3)).title()
    vpage = page.replace("<title>", f"<title>{title} - ", 1) if "<title>" in page else page
    open(os.path.join(OUT, "demo-shared", f"page{k}.html"), "w").write(vpage)
    nh = 6 + (k * 7) % 23 + k // 5
    handlers, arms = [], []
    for i in range(nh):
        c1, c2 = rnd.randrange(3, 2**31) | 1, rnd.randrange(3, 2**31) | 1
        rot, iters, mod, x = rnd.randrange(1, 31), rnd.randrange(20, 400), rnd.randrange(3, 50), rnd.randrange(0, 3)
        w = " ".join(rnd.choice(WORDS) for _ in range(rnd.randrange(3, 12)))
        col = rnd.choice(["id", "created_at", "length(text)"])
        body = f'''
#[inline(never)]
fn h{i}(c: &Connection) -> String {{
    let mut acc: u64 = {c1}u64 ^ {k};
    for j in 0..{iters}u64 {{
        acc = acc.wrapping_mul({c2}u64).rotate_left({rot}) ^ (j + {i});
    }}
    let n: i64 = c
        .query_row("SELECT count(*) FROM items WHERE ({col}) % {mod} = {x}", [], |r| r.get(0))
        .unwrap_or(0);
    format!("{w} {{}} {{}}", acc, n)
}}
'''
        handlers.append(body)
        arms.append(f'        "/r{i}" => Some(h{i}(c)),')
    extra = "".join(handlers) + f'''
fn extra(path: &str, c: &Connection) -> Option<String> {{
    match path {{
{chr(10).join(arms)}
        _ => None,
    }}
}}
'''
    v = main.replace("../../demo-shared/page.html", f"../../demo-shared/page{k}.html")
    v = v.replace('fn json<T', extra + "\nfn json<T", 1)
    v = v.replace('        _ => Ok(Response::builder()',
                  '        (Method::GET, p) if p.starts_with("/r") && with_db(|c| extra(p, c)).is_some() => {\n'
                  '            let body = with_db(|c| extra(p, c)).unwrap();\n'
                  '            Ok(Response::builder().status(StatusCode::OK).header("content-type", "text/plain").body(body.into())?)\n'
                  '        }\n'
                  '        _ => Ok(Response::builder()', 1)
    assert "extra(p, c)" in v and "page%d.html" % k in v
    open(os.path.join(d, "src", "main.rs"), "w").write(v)
print("generated", N, "variants in", OUT)
if LEAN:
    print("build with: LIBSQLITE3_FLAGS=\"%s\" and -O3 replaced by -Os in CFLAGS_wasm32_wasip2" % LEAN_SQLITE)
