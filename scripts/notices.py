#!/usr/bin/env python3
"""Write THIRD-PARTY-NOTICES: the licenses of what ships with spared.

    python3 -I scripts/notices.py > THIRD-PARTY-NOTICES      # regenerate
    python3 -I scripts/notices.py --check                    # fail if the committed file is stale

Part 1 lists every Rust crate linked into the `spared` binary: the normal dependencies of the
`spared` package for the four release targets (build and dev dependencies are left out), with each
crate's license expression, repository, and the license and notice files found in its package.
Part 2 covers what is compiled into an app component: the guest crates of the example apps that
are shipped (the release workflow and the Docker image include them), SQLite, and wasi-libc with
the parts of it that carry their own license. Texts are printed once and referred to by number.

The input is `cargo metadata` over Cargo.lock, plus the files in scripts/notices/. The output has
no timestamp: the same lock files and the same registry sources always give the same bytes.
Offline: set CARGO_NET_OFFLINE=true (the crates must already be in ~/.cargo/registry). Needs Python 3.9.
"""
import hashlib
import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
HOST_TARGETS = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
]
GUEST_MANIFESTS = ["examples/notes/Cargo.toml", "apps/shared-list/Cargo.toml", "apps/demo-spared/Cargo.toml"]
GUEST_TARGET = "wasm32-wasip2"
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|unlicense|notice|copyright)([-_.].*)?$", re.I)
# REUSE-style LICENSES/<SPDX id>.txt files. A dual-licensed crate ships the text of the option we do
# not use too (ittapi-sys: GPL-2.0-only OR BSD-3-Clause); only the permissive text is redistributed.
COPYLEFT_FILE = re.compile(r"^(a|l)?gpl-", re.I)

SQLITE_NOTICE = """SQLite (https://www.sqlite.org) is in the public domain. Its source files say:

  The author disclaims copyright to this source code.  In place of
  a legal notice, here is a blessing:

     May you do good and not evil.
     May you find forgiveness for yourself and forgive others.
     May you share freely, never taking more than you give.

The C source is compiled into every app component from the `libsqlite3-sys` crate (MIT, listed
below) with the wasi-sdk clang."""

WASI_LIBC_FILES = [
    ("LICENSE", "wasi-libc: summary of its licenses"),
    ("LICENSE-APACHE-LLVM", "wasi-libc, and LLVM compiler-rt: Apache License 2.0 with LLVM Exceptions"),
    ("LICENSE-APACHE", "wasi-libc: Apache License 2.0"),
    ("LICENSE-MIT", "wasi-libc: MIT License"),
    ("musl-COPYRIGHT", "musl libc, inside wasi-libc: MIT, with the notices of the files it includes"),
    ("cloudlibc-LICENSE", "cloudlibc, inside wasi-libc: BSD-2-Clause"),
    ("musl-fts-COPYING", "musl-fts, inside wasi-libc: BSD-3-Clause"),
]

WASI_LIBC_NOTICE = """The C code that runs inside an app component is built with wasi-sdk 25 (clang) and linked with
wasi-libc and the Rust standard library, so a component holds code from both:

- wasi-libc (https://github.com/WebAssembly/wasi-libc, revision 574b88da4815, the one in wasi-sdk 25)
  is multi-licensed under the Apache License 2.0 with LLVM Exceptions, the Apache License 2.0 and the
  MIT license. It also contains musl libc (MIT), cloudlibc (BSD-2-Clause), musl-fts (BSD-3-Clause),
  dlmalloc (CC0, a public domain dedication) and emmalloc (MIT, notice in its source file).
- The compiler runtime (LLVM compiler-rt builtins) is Apache-2.0 with LLVM Exceptions.
- The Rust standard library and core are MIT OR Apache-2.0.

The wasi-sdk itself is not redistributed: `spared setup` downloads it from its release page."""


def metadata(manifest, target, extra=()):
    env = dict(os.environ)
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", target, *extra, "--manifest-path", os.path.join(ROOT, manifest)],
        check=True, capture_output=True, text=True, cwd=ROOT, env=env,
    ).stdout
    return json.loads(out)


def normal_closure(meta, root_name=None):
    """Packages reachable from the workspace's root (or the package named `root_name`) through normal dependencies."""
    pkgs = {p["id"]: p for p in meta["packages"]}
    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    if root_name:
        roots = [p["id"] for p in meta["packages"] if p["name"] == root_name and p["source"] is None]
    else:
        roots = list(meta["workspace_members"])
    seen, stack = set(), list(roots)
    while stack:
        i = stack.pop()
        if i in seen:
            continue
        seen.add(i)
        for d in nodes[i]["deps"]:
            if any(k["kind"] is None for k in d["dep_kinds"]):
                stack.append(d["pkg"])
    return [pkgs[i] for i in seen if pkgs[i]["source"] is not None]


def read_text(path):
    with open(path, "rb") as f:
        return f.read().decode("utf-8", errors="replace").replace("\r\n", "\n").strip("\n")


def package_texts(pkg):
    d = os.path.dirname(pkg["manifest_path"])
    out = []
    for name in sorted(os.listdir(d)):
        p = os.path.join(d, name)
        if os.path.isfile(p) and LICENSE_FILE.match(name):
            out.append(read_text(p))
    licenses = os.path.join(d, "LICENSES")
    if os.path.isdir(licenses):
        for name in sorted(os.listdir(licenses)):
            p = os.path.join(licenses, name)
            if os.path.isfile(p) and not COPYLEFT_FILE.match(name):
                out.append(read_text(p))
    return out


def gather(manifests, targets, root_name=None, extra=()):
    found = {}
    for m in manifests:
        for t in targets:
            for p in normal_closure(metadata(m, t, extra), root_name):
                found[(p["name"], p["version"])] = p
    return found


class Texts:
    """License texts, printed once each and referred to by number."""

    def __init__(self):
        self.by_key = {}
        self.order = []

    def ref(self, title, text):
        key = hashlib.sha256(" ".join(text.split()).encode()).hexdigest()
        if key not in self.by_key:
            self.by_key[key] = len(self.order) + 1
            self.order.append((title, text))
        return self.by_key[key]


def crate_section(found, texts):
    lines = []
    for (name, version), p in sorted(found.items()):
        refs = sorted({texts.ref(f"{name} {version}", t) for t in package_texts(p)})
        lines.append(f"{name} {version}")
        lines.append(f"  License: {p['license'] or 'see the license file'}")
        if p.get("repository"):
            lines.append(f"  Source: {p['repository']}")
        lines.append("  Texts: " + (", ".join(f"[{r}]" for r in refs) if refs else "none in the package; the license named above applies"))
        lines.append("")
    return lines


def build():
    texts = Texts()
    # The default features only: `lab` (native SQLite, test commands) is never in a release.
    host = gather(["Cargo.toml"], HOST_TARGETS, "spared", ["--no-default-features"])
    guest = gather(GUEST_MANIFESTS, [GUEST_TARGET])
    # A crate in both lists is printed with the host list only.
    guest = {k: v for k, v in guest.items() if k not in host}
    out = [
        "THIRD-PARTY NOTICES",
        "",
        "spared is licensed under the Apache License 2.0 (LICENSE). This file lists the third-party",
        "software that ships with it, with its licenses. It is generated by scripts/notices.py; do not",
        "edit it by hand (CONTRIBUTING.md says how to regenerate it).",
        "",
        "=" * 78,
        "PART 1. Rust crates linked into the spared binary",
        "=" * 78,
        "",
        f"{len(host)} crates: the normal dependencies of the spared package for "
        + ", ".join(HOST_TARGETS)
        + ".",
        "",
    ]
    out += crate_section(host, texts)
    out += [
        "=" * 78,
        "PART 2. Code compiled into app components",
        "=" * 78,
        "",
        "The example apps shipped with spared (the example .wasm files in a release and the apps in",
        "the Docker image) are components. Apps you build with `spared new` contain the same kind of",
        "code. SQLite, wasi-libc and the Rust crates below are inside each one.",
        "",
        SQLITE_NOTICE,
        "",
        WASI_LIBC_NOTICE,
        "",
    ]
    for fname, title in WASI_LIBC_FILES:
        n = texts.ref(title, read_text(os.path.join(ROOT, "scripts", "notices", fname)))
        out.append(f"  {title}: [{n}]")
    out += [
        "",
        f"Rust crates in the example apps that the host binary does not also link ({len(guest)}):",
        "",
    ]
    out += crate_section(guest, texts)
    out += ["=" * 78, "LICENSE TEXTS", "=" * 78, ""]
    for i, (title, text) in enumerate(texts.order, 1):
        out += [f"[{i}] First seen in: {title}", "-" * 78, text, ""]
    return "\n".join(out).rstrip("\n") + "\n"


def main():
    text = build()
    if sys.argv[1:] == ["--check"]:
        path = os.path.join(ROOT, "THIRD-PARTY-NOTICES")
        have = open(path, encoding="utf-8").read() if os.path.exists(path) else ""
        if have != text:
            sys.stderr.write("THIRD-PARTY-NOTICES is out of date. Fix: python3 -I scripts/notices.py > THIRD-PARTY-NOTICES\n")
            raise SystemExit(1)
        return
    if sys.argv[1:]:
        raise SystemExit("usage: notices.py [--check]")
    sys.stdout.write(text)


if __name__ == "__main__":
    main()
