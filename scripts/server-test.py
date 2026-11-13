#!/usr/bin/env python3
"""Integration tests that go through `spared serve` (server.rs): routing, the admin API's token,
Host and Origin checks, operator limits, redeploys, traps, large responses, a spinning app, slow
clients, logs, file modes, evict, backups (backup, list, restore, undo, retention, path
traversal, backup under write load, a hostile database), clients that disconnect mid-request or
never read, admin calls (backup, restore, delete, redeploy, evict) whose client goes away, deploys of
components that do not link (the app must stay as it was, also after a restart), data trees
nested 15,000 deep, a data tree whose directories are swapped for links during a backup, compile bombs (small components that are very expensive to compile), a compiled component from another wasmtime version,
`spared bench`, the health check, the metrics endpoint, the access log (and what it must never
hold), the data directory's format stamp, the per-app caps against an app that lies to the host (a file renamed onto itself, a hard link unlinked, a stale stream, streams that outlive their descriptor), the request-body budget, a component store that cannot hard link, and refusals at start (a taken port, a data dir in use).

Run from anywhere:  python3 -I scripts/server-test.py
Needs: target/release/spared (or $SPARED_BIN), examples/dist (built by scripts/build-examples.sh
when missing, which needs wasi-sdk) and probes/hostile-app (built here when missing; needs the
wasm32-wasip2 target only). Uses scratch ports and a temp data dir, and gives the server its own
$HOME so ~/.spared/token is never touched. Takes about a minute and a half.
"""
import base64
import http.client
import json
import os
import random
import re
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("SPARED_BIN", os.path.join(ROOT, "target/release/spared"))
DIST = os.path.join(ROOT, "examples/dist")
HOSTILE_DIR = os.path.join(ROOT, "probes/hostile-app")
HOSTILE = os.path.join(HOSTILE_DIR, "target/wasm32-wasip2/release/hostile-app.wasm")
CLIENT_TIMEOUT = 2

# A 53-byte component with no core code that exports an empty instance named like the HTTP
# handler. It passes the name checks and the import checks and fails only when the host links it
# ("instance export `wasi:http/incoming-handler@0.2.12` does not have export `handle`").
LINK_FAILS_EMPTY_HANDLER = (b"\0asm\x0d\0\x01\0" + bytes([0x05, 0x03, 0x01, 0x01, 0x00])
                            + bytes([0x0b, 38, 0x01, 0x00, 32]) + b"wasi:http/incoming-handler@0.2.0" + bytes([0x05, 0x00, 0x00]))
# A component whose handler is fine and which imports `wasi:cli/nope@0.2.0`: every `wasi:cli/` name
# is allowed by the import check, and no such interface exists in the linker.
LINK_FAILS_UNKNOWN_IMPORT = bytes.fromhex(
    "0061736d0d000100070e01420201400001000400016601000a18010013776173693a636c692f6e6f706540302e322e300500"
    "072e014202040010696e636f6d696e672d726571756573740301040011726573706f6e73652d6f7574706172616d03010a1a"
    "010015776173693a687474702f747970657340302e322e340501062a0203000110696e636f6d696e672d7265717565737403"
    "000111726573706f6e73652d6f7574706172616d01310061736d0100000001060160027f7f0003020100070a010668616e64"
    "6c6500000a040102000b0009046e616d650002016d0204010000000720036902690340020772657175657374040c72657370"
    "6f6e73652d6f7574050100060c01000001000668616e646c650806010000000006050d010101000668616e646c6501000b26"
    "010020776173693a687474702f696e636f6d696e672d68616e646c657240302e322e34050200004f0e636f6d706f6e656e74"
    "2d6e616d65010600110100016d0106001201000169010a0101000668616e646c65010e03020204697265710304726f757401"
    "12050300046e6f706501057479706573020168")
KEEP = {}
INTEGRITY = "copied, sha256 verified; database not opened by the host"
passed = 0


def ok(name):
    global passed
    passed += 1
    print(f"ok - {name}", flush=True)


def check(cond, name, detail=""):
    if not cond:
        raise SystemExit(f"FAIL - {name} {detail}")
    ok(name)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def call(port, method, path, host, headers=None, body=None, timeout=30):
    c = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    h = {"Host": host}
    h.update(headers or {})
    t = time.monotonic()
    c.request(method, path, body=body, headers=h)
    r = c.getresponse()
    data = r.read()
    c.close()
    return r.status, dict(r.getheaders()), data, time.monotonic() - t


def kind(data):
    try:
        return json.loads(data)["error"]["kind"]
    except Exception:
        return None


class Server:
    def __init__(self, tmp, *extra, pidns=False, nofile=None):
        self.tmp, self.data = tmp, os.path.join(tmp, "data")
        self.pub, self.adm = free_port(), free_port()
        self.home = os.path.join(tmp, "home")
        os.makedirs(self.home, exist_ok=True)
        self.log = open(os.path.join(tmp, "serve.log"), "ab")
        env = dict(os.environ, HOME=self.home)
        env.pop("SPARED_ADMIN_TOKEN", None)
        env.pop("SPARED_PATH_ROUTING", None)
        timeout = [] if "--client-timeout-secs" in extra else ["--client-timeout-secs", str(CLIENT_TIMEOUT)]
        args = [BIN, "serve", "--data-dir", self.data, "--listen", f"127.0.0.1:{self.pub}",
                "--admin-listen", f"127.0.0.1:{self.adm}", *timeout, *extra]
        if pidns:
            args = ["unshare", "--pid", "--fork", "--mount-proc", *args]
        limit = None
        if nofile:
            import resource

            def limit():
                resource.setrlimit(resource.RLIMIT_NOFILE, nofile)
        self.p = subprocess.Popen(args, stdout=self.log, stderr=self.log, env=env, preexec_fn=limit)
        self.pidns = pidns
        for _ in range(100):
            if self.p.poll() is not None:
                raise SystemExit("FAIL - server exited during startup")
            # The ports open before the token is written and copied to ~/.spared/token, so wait
            # for an HTTP answer: the server answers only after both are done.
            try:
                c = http.client.HTTPConnection("127.0.0.1", self.pub, timeout=1)
                c.request("GET", "/healthz")
                ok = c.getresponse().status == 200
                c.close()
                if ok:
                    break
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(0.1)
        self.token = open(os.path.join(self.data, "admin-token")).read().strip()
        self.auth = {"Authorization": f"Bearer {self.token}"}

    def admin(self, method, path, headers=None, body=None, host=None):
        h = dict(self.auth)
        h.update(headers or {})
        return call(self.adm, method, path, host or f"127.0.0.1:{self.adm}", h, body)

    def deploy(self, name, wasm, manifest):
        with open(wasm, "rb") as f:
            body = f.read()
        m = base64.b64encode(manifest.encode()).decode()
        st, _, data, _ = self.admin("PUT", f"/v1/apps/{name}", {"x-spared-manifest": m}, body)
        return st, json.loads(data)

    def app(self, name, path, method="GET", body=None, headers=None, timeout=30):
        return call(self.pub, method, path, f"{name}.localhost:{self.pub}", headers, body, timeout)

    def server_pid(self):
        """The server's pid on this host (under `unshare --fork` it is the wrapper's child)."""
        if not self.pidns:
            return self.p.pid
        for _ in range(50):
            kids = subprocess.run(["pgrep", "-P", str(self.p.pid)], capture_output=True, text=True).stdout.split()
            if kids:
                return int(kids[0])
            time.sleep(0.1)
        raise SystemExit("FAIL - no server process under unshare")

    def stop(self):
        self.p.terminate()
        self.p.wait(10)
        self.log.close()


def _leb(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def _sleb(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if (n == 0 and not b & 0x40) or (n == -1 and b & 0x40):
            out.append(b)
            return bytes(out)
        out.append(b | 0x80)


def compile_bomb(local_count, statements):
    """A component holding one core module with one function that has `local_count` i32 locals and
    `statements` conditional assignments in a loop. It is small and valid, and the compiler's
    memory and time grow much faster than its size: 8,000 of each is 96 KB and does not finish
    within 5 GB; 4,000 of each is 48 KB, about 10 s and half a gigabyte."""
    body = bytearray(_leb(1) + _leb(local_count) + b"\x7f" + b"\x03\x40")
    for k in range(statements):
        body += b"\x20" + _leb(k % local_count) + b"\x04\x40\x41" + _sleb(k) + b"\x21" + _leb((k * 7 + 1) % local_count) + b"\x0b"
    body += b"\x0c\x00\x0b\x0b"
    code = _leb(1) + _leb(len(body)) + body
    module = b"\0asm\x01\0\0\0" + b"\x01\x04\x01\x60\x00\x00" + b"\x03\x02\x01\x00" + b"\x0a" + _leb(len(code)) + code
    return b"\0asm\x0d\0\x01\0" + b"\x01" + _leb(len(module)) + module


def build_inputs():
    env = dict(os.environ)
    env.pop("CARGO_TARGET_DIR", None)
    if not os.path.isfile(os.path.join(DIST, "notes/app.wasm")):
        subprocess.run(["sh", os.path.join(ROOT, "scripts/build-examples.sh")], check=True, env=env)
    subprocess.run(["cargo", "build", "--release", "--target", "wasm32-wasip2"], cwd=HOSTILE_DIR, check=True, env=env)


def hostile_manifest(name="hostile", **limits):
    lines = "".join(f"{k} = {v}\n" for k, v in limits.items())
    return f'name = "{name}"\n[grants]\ndata = true\nfetch = ["example.com"]\n[limits]\n{lines}'


def sha256_file(path):
    import hashlib
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def make_deep(path, depth):
    """Nest `depth` directories under `path` with one descriptor open at a time and short names
    (mkdirat and openat, the calls the WASI layer makes), past PATH_MAX; a file at the bottom."""
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    for _ in range(depth):
        os.mkdir("d", dir_fd=fd)
        nfd = os.open("d", os.O_RDONLY | os.O_DIRECTORY, dir_fd=fd)
        os.close(fd)
        fd = nfd
    f = os.open("f", os.O_WRONLY | os.O_CREAT, 0o600, dir_fd=fd)
    os.write(f, b"bottom")
    os.close(f)
    os.close(fd)


def raw(port, data, read=True, timeout=5):
    c = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    c.sendall(data)
    got = b""
    if read:
        try:
            while True:
                d = c.recv(65536)
                if not d:
                    break
                got += d
                if b"\r\n\r\n" in got:
                    break
        except OSError:
            pass
    c.close()
    return got


def mode(path):
    return oct(os.stat(path).st_mode & 0o777)


def main():
    if not os.access(BIN, os.X_OK):
        raise SystemExit(f"server-test: {BIN} not found; run cargo build --release -p spared")
    build_inputs()
    tmp = tempfile.mkdtemp(prefix="spared-server-test.")
    s = None
    try:
        s = Server(tmp, "--max-backups-per-app", "4", "--max-app-entries", "1000000")
        run(s, tmp)
        s.stop()
        s = Server(tmp, "--allow-path-routing", "--max-cpu-ms", "3000")
        restart(s, tmp)
        signals(tmp)
        descriptors(tmp)
        resource_caps(tmp)
        fs_books(tmp)
        fs_books(tmp, per_app=True)
        per_app_limits(tmp)
        egress_budget(tmp)
        body_budget(tmp)
        store_without_links(tmp)
        startup_refusals(tmp)
        compile_bombs(tmp)
        backup_races(tmp)
        host_work(tmp)
    except BaseException:
        log = os.path.join(tmp, "serve.log")
        if os.path.exists(log):
            sys.stderr.write("--- server log ---\n" + open(log, errors="replace").read()[-3000:])
        raise
    finally:
        if s and s.p.poll() is None:
            s.stop()
        if os.environ.get("KEEP_TMP") != "1":
            shutil.rmtree(tmp, ignore_errors=True)
    print(f"server-test: PASS ({passed} checks)")


def run(s, tmp):
    # token files and modes
    check(mode(os.path.join(s.data, "admin-token")) == "0o600", "admin-token is 0600")
    check(mode(s.data) == "0o700", "data dir is 0700")
    user = os.path.join(s.home, ".spared/token")
    check(open(user).read().strip() == s.token and mode(user) == "0o600", "serve copies the token to ~/.spared/token (0600)")

    # admin API guards
    st, _, d, _ = call(s.adm, "GET", "/v1/apps", f"127.0.0.1:{s.adm}")
    check(st == 401 and kind(d) == "unauthorized", "admin without a token is 401", d)
    check(b"admin-token" in d, "401 names where the token is", d)
    st, _, d, _ = call(s.adm, "GET", "/v1/apps", f"127.0.0.1:{s.adm}", {"Authorization": "Bearer nope"})
    check(st == 401, "admin with a wrong token is 401")
    for host in ["attacker.example", f"attacker.example:{s.adm}", "127.0.0.1.nip.io"]:
        st, _, d, _ = s.admin("GET", "/v1/apps", host=host)
        check(st == 403 and kind(d) == "bad_host", f"admin Host '{host}' is rejected", d)
    for host in [f"localhost:{s.adm}", f"127.0.0.1:{s.adm}"]:
        st, _, _, _ = s.admin("GET", "/v1/apps", host=host)
        check(st == 200, f"admin Host '{host}' with the token is 200")
    st, _, d, _ = s.admin("GET", "/v1/apps", {"Origin": f"http://127.0.0.1:{s.adm}"})
    check(st == 403 and kind(d) == "origin_refused", "admin request with an Origin header is rejected", d)
    check(s.data.encode() not in d and b"<data-dir>/admin-token" in call(s.adm, "GET", "/v1/apps", f"127.0.0.1:{s.adm}")[2],
          "a 401 does not reveal the data dir's path")
    got = raw(s.adm, f"GET /v1/apps HTTP/1.1\r\nHost: 127.0.0.1\r\nHost: attacker.example\r\nAuthorization: Bearer {s.token}\r\nConnection: close\r\n\r\n".encode())
    check(got.startswith(b"HTTP/1.1 403") and b"bad_host" in got, "an admin request with two Host headers is rejected", got[:200])

    # the admin API is versioned: /v1/apps, and the unversioned path is not served
    st, _, d, _ = s.admin("GET", "/apps")
    check(st == 404 and kind(d) == "no_route" and b"/v1/" in d, "the admin API is under /v1/: /apps is a 404 that says so", d)
    st, _, d, _ = s.admin("GET", "/v1/apps")
    check(st == 200 and "apps" in json.loads(d), "GET /v1/apps lists apps", d)

    # hostile manifest limits
    for field, value in [("cpu_ms", 10**12), ("memory_mb", 10**6), ("deadline_ms", 10**12), ("max_outbound", 4000000000)]:
        st, v = s.deploy("hostile", HOSTILE, hostile_manifest(**{field: value}))
        e = v.get("error", {})
        check(st == 400 and e.get("kind") == "limit_too_high" and field in e.get("message", "") and "--max-" in e.get("fix", ""),
              f"deploy with {field} = {value} is refused with limit_too_high", v)

    for typo in ['nmae = "x"\n', '[grants]\ndat = true\n', '[limits]\nmemory = 8\n']:
        st, v = s.deploy("hostile", HOSTILE, 'name = "hostile"\n' + typo)
        e = v.get("error", {})
        key = typo.split("=")[0].split()[-1]
        check(st == 400 and e.get("kind") == "bad_manifest" and key in e.get("message", "") and "valid keys:" in e.get("fix", ""),
              f"deploy with unknown manifest key {key} is refused with bad_manifest", v)

    st, v = s.deploy("hostile", HOSTILE, 'schema = 2\nname = "hostile"\nsome_new_key = true\n')
    e = v.get("error", {})
    check(st == 400 and e.get("kind") == "unsupported_schema" and "schema = 2" in e.get("message", "") and "upgrade spared" in e.get("fix", ""),
          "deploy with a future manifest schema is refused with unsupported_schema, not with whatever new key comes first", v)
    st, v = s.deploy("hostile", HOSTILE, 'name = "hostile"\n[secrets]\nAPI_KEY = "hunter2"\n')
    e = v.get("error", {})
    check(st == 400 and e.get("kind") == "secrets_not_supported" and "reserved" in e.get("message", "") and "hunter2" not in json.dumps(v),
          "deploy with a [secrets] table is refused as reserved, without echoing the value", v)

    st, v = s.deploy("notes", os.path.join(DIST, "notes/app.wasm"), open(os.path.join(DIST, "notes/spared.toml")).read())
    check(st == 200 and v["data_kept"] is False, "deploy notes", v)
    st, v = s.deploy("hostile", HOSTILE, hostile_manifest(cpu_ms=1000, deadline_ms=10000))
    check(st == 200, "deploy hostile-app with sane limits", v)

    # routing
    st, _, _, _ = s.app("notes", "/")
    check(st == 200, "GET notes.localhost/ is 200")
    st, _, d, _ = s.app("nope", "/")
    check(st == 404 and kind(d) == "no_such_app", "unknown app is 404 no_such_app", d)
    st, _, d, _ = call(s.pub, "GET", "/_/notes/", f"127.0.0.1:{s.pub}")
    check(st == 404 and kind(d) == "path_routing_off" and b"--allow-path-routing" in d, "/_/<app>/ is off by default and names the flag", d)

    # traps
    st, _, d, t = s.app("hostile", "/panic")
    check(st == 500 and kind(d) == "app_trap" and t < 2, f"panic is 500 app_trap in {t:.2f}s", d)
    check(b"wasm" not in d and b"backtrace" not in d, "the trap response carries no host detail", d)
    st, _, d, _ = s.app("hostile", "/")
    check(st == 200 and d == b"ok\n", "the app answers again after a panic")

    # a real out-of-memory (an allocation past memory_mb) is reported as memory_limit, not as a
    # panic, and the app answers again afterwards
    st, v = s.deploy("capped", HOSTILE, hostile_manifest("capped", memory_mb=16, cpu_ms=1000, deadline_ms=10000))
    assert st == 200, v
    st, _, d, _ = s.app("capped", "/alloc?mb=4")
    check(st == 200 and b"allocated 4194304 bytes" in d, "an allocation inside memory_mb works", (st, d[:200]))
    st, _, d, t = s.app("capped", "/alloc?mb=64")
    check(st == 500 and kind(d) == "memory_limit" and b"16 MB" in d and t < 3, f"an allocation past memory_mb is 500 memory_limit in {t:.2f}s", (st, d[:300]))
    st, _, d, _ = s.app("capped", "/alloc?mb=4")
    check(st == 200, "the app answers again after hitting its memory limit")
    st, _, d, _ = s.app("capped", "/panic")
    check(st == 500 and kind(d) == "app_trap", "a panic on the same app is still app_trap", d)
    st, _, d, _ = s.app("capped", "/alloc?mb=4")
    check(st == 200, "and a request after the panic does not inherit the memory flag")
    s.admin("DELETE", "/v1/apps/capped")

    # large responses
    st, _, d, t = s.app("hostile", "/big?mb=5")
    check(st == 200 and len(d) == 5 << 20 and t < 3, f"5 MB response arrives whole in {t:.2f}s", (st, len(d)))
    st, _, d, t = s.app("hostile", "/big?mb=20")
    check(st == 502 and kind(d) == "response_too_large" and t < 3, f"20 MB response fails with response_too_large in {t:.2f}s", d)
    st, _, _, _ = s.app("hostile", "/")
    check(st == 200, "the app answers again after an oversized response")

    # a spinning app does not hold up other apps or the admin API
    s.app("notes", "/api/notes")
    spin = {}

    def spinner():
        spin["r"] = s.app("hostile", "/spin")

    th = threading.Thread(target=spinner)
    th.start()
    time.sleep(0.1)
    worst = 0.0
    samples = 0
    while th.is_alive() and samples < 40:
        for f in (lambda: s.app("notes", "/api/notes"), lambda: s.admin("GET", "/v1/apps")):
            st, _, _, t = f()
            assert st == 200, st
            worst = max(worst, t)
            samples += 1
        time.sleep(0.01)
    th.join()
    check(samples >= 10 and worst < 0.1, f"while an app spins, {samples} requests to another app and the admin API all answer within 100 ms (worst {worst * 1000:.1f} ms)")
    st, _, d, t = spin["r"]
    check(st == 508 and kind(d) == "cpu_limit" and t < 3, f"the spinning request is stopped by cpu_ms ({t:.2f}s)", d)

    # redeploy keeps data, retires the old instance, and requests during it keep working
    st, _, d, _ = s.app("notes", "/api/notes", "POST", json.dumps({"text": "kept across redeploy"}), {"content-type": "application/json"})
    check(st == 201, "POST a note", d)
    results, stop = [], threading.Event()

    def reader():
        while not stop.is_set():
            st, _, d, _ = s.app("notes", "/api/notes")
            results.append((st, d))

    readers = [threading.Thread(target=reader) for _ in range(3)]
    for r in readers:
        r.start()
    versions = []
    manifest = open(os.path.join(DIST, "notes/spared.toml")).read()
    for _ in range(4):
        st, v = s.deploy("notes", os.path.join(DIST, "notes/app.wasm"), manifest)
        assert st == 200 and v["data_kept"] is True and "spared rm" in v["note"], v
        versions.append(v["version"])
        time.sleep(0.05)
    stop.set()
    for r in readers:
        r.join()
    check(versions == [2, 3, 4, 5], "redeploys report data_kept and bump the version", versions)
    bad = [x for x in results if x[0] != 200 or b"kept across redeploy" not in x[1]]
    check(results and not bad, f"{len(results)} reads during 4 redeploys all see the note", bad[:2])
    st, _, d, _ = s.app("notes", "/api/notes")
    check([n["text"] for n in json.loads(d)].count("kept across redeploy") == 1, "the note is there exactly once after the redeploys")
    st, _, d, _ = s.admin("GET", "/v1/apps/notes")
    check(json.loads(d)["wakes"] <= 1, "only the newest instance woke after the last redeploy", d)

    disconnects(s)

    # logs survive invalid UTF-8
    with open(os.path.join(s.data, "apps/hostile/app.log"), "ab") as f:
        f.write(b"\xff\xfe\n")
    st, _, d, _ = s.admin("GET", "/v1/apps/hostile/logs?lines=100")
    text = d.decode()
    check(st == 200 and "panic on purpose" in text and "�" in text, "logs with invalid UTF-8 still show the panic", text[-200:])

    # file modes
    check(mode(os.path.join(s.data, "apps/notes/data/app.db")) == "0o600", "app.db is 0600")
    check(mode(os.path.join(s.data, "apps/notes/component.cwasm")) == "0o600", "component.cwasm is 0600")
    check(mode(os.path.join(s.data, "apps/notes")) == "0o700", "app dir is 0700")

    component_store(s, manifest)

    backups(s, manifest)

    bad_backup_ids(s)

    slow_clients(s)

    never_read(s)

    admin_aborts(s)

    atomic_deploy(s, manifest)

    deep_trees(s)

    observability(s, tmp)

    st, _, _, _ = s.admin("DELETE", "/v1/apps/hostile")
    check(st == 200 and not os.path.exists(os.path.join(s.data, "apps/hostile")), "delete removes the app")
    st, v = s.deploy("hostile", HOSTILE, hostile_manifest(cpu_ms=1000))
    check(st == 200 and v["data_kept"] is False, "a deploy after delete starts with no data", v)


def component_store(s, notes_manifest):
    comps = os.path.join(s.data, "components")
    files = lambda: sorted(f for f in os.listdir(comps) if f.endswith(".cwasm"))
    ino = lambda p: os.stat(p).st_ino
    wasm = os.path.join(DIST, "notes/app.wasm")
    v = json.loads(s.admin("GET", "/v1/apps/notes")[2])
    h = v["component_sha256"]
    stored = os.path.join(comps, h + ".cwasm")
    check(len(h) == 64 and os.path.isfile(stored), "the app's compiled component is stored under its sha256", h)
    check(ino(stored) == ino(os.path.join(s.data, "apps/notes/component.cwasm")), "apps/notes/component.cwasm is a link to the stored file")
    before = files()
    # the same bytes under another name: same stored file, nothing new on disk, limits stay per app
    second = notes_manifest.replace('name = "notes"', 'name = "notes2"').replace("memory_mb = 64", "memory_mb = 32")
    st, v2 = s.deploy("notes2", wasm, second)
    check(st == 200 and v2["component_sha256"] == h and v2["component_reused"] is True, "identical bytes under a new name reuse the component", v2)
    check(files() == before, "no second compiled file is written", files())
    lim = lambda n: json.loads(s.admin("GET", f"/v1/apps/{n}")[2])["limits"]["memory_mb"]
    check((lim("notes"), lim("notes2")) == (64, 32), "limits stay per app when the component is shared", (lim("notes"), lim("notes2")))
    check(ino(os.path.join(s.data, "apps/notes2/component.cwasm")) == ino(stored), "both apps link to one file")
    st, _, d, _ = s.app("notes2", "/api/notes")
    check(st == 200 and b"kept across redeploy" not in d, "the second app serves with its own empty data", d)
    st, _, d, _ = s.app("notes", "/api/notes")
    check(st == 200 and b"kept across redeploy" in d, "the first app still serves its own data")
    st, _, _, _ = s.admin("DELETE", "/v1/apps/notes2")
    check(st == 200 and files() == before and os.path.isfile(stored), "deleting one user keeps the shared component")
    st, _, d, _ = s.app("notes", "/api/notes")
    check(st == 200, "the remaining app still serves after the other was deleted")
    # a different component: new file; redeploying away from it removes the old one when unused
    other = os.path.join(DIST, "list/app.wasm")
    om = open(os.path.join(DIST, "list/spared.toml")).read().replace('name = "list"', 'name = "tmp-store"')
    st, v3 = s.deploy("tmp-store", other, om)
    check(st == 200 and v3["component_sha256"] != h and len(files()) == len(before) + 1, "a different component is stored once", v3)
    st, v4 = s.deploy("tmp-store", wasm, om)
    check(st == 200 and v4["component_sha256"] == h and files() == before, "redeploying away from a component removes it when no app uses it", files())
    s.admin("DELETE", "/v1/apps/tmp-store")
    check(files() == before, "a deleted app leaves no extra component behind")
    junk = os.path.join(comps, "0" * 64 + ".cwasm")
    open(junk, "wb").write(b"orphan")
    open(os.path.join(comps, "x.cwasm.tmp"), "wb").write(b"partial")


ACCESS_FIELDS = {"time", "listener", "app", "method", "path", "status", "duration_ms", "request_id"}


def access_lines(text):
    """The access-log lines in the server's output (stdout and stderr share one file here)."""
    return [json.loads(line) for line in text.splitlines() if line.startswith('{"time":')]


def observability(s, tmp):
    logf = os.path.join(tmp, "serve.log")
    pub_host, adm_host = f"127.0.0.1:{s.pub}", f"127.0.0.1:{s.adm}"

    # the data directory's format stamp
    stamp_file = os.path.join(s.data, "format.json")
    stamp = json.load(open(stamp_file))
    check(stamp["format"] == 1 and stamp["created_by"] == stamp["last_opened_by"] and mode(stamp_file) == "0o600",
          "a new data dir is stamped with its format and the version that made it (0600)", stamp)

    # GET /healthz on the public listener
    st, h, d, _ = call(s.pub, "GET", "/healthz", pub_host)
    v = json.loads(d)
    check(st == 200 and sorted(v) == ["status", "uptime_seconds", "version"] and v["status"] == "ok"
          and re.match(r"^\d+\.\d+\.\d+", v["version"]) and isinstance(v["uptime_seconds"], int),
          "GET /healthz is 200 with status, version and uptime only", d)
    names = [a["name"].encode() for a in json.loads(s.admin("GET", "/v1/apps")[2])["apps"]]
    check(names and not any(n in d for n in names) and s.data.encode() not in d and s.token.encode() not in d,
          "/healthz names no app, no path and no token", d)
    check(re.fullmatch(r"[0-9a-f]{16}", h.get("x-spared-request-id", "")), "a response carries a request id header", h)
    st, _, d, _ = call(s.pub, "HEAD", "/healthz", pub_host)
    check(st == 200 and d == b"", "HEAD /healthz is 200")
    st, _, d, _ = call(s.pub, "POST", "/healthz", pub_host, body=b"x")
    check(st == 404 and kind(d) == "no_such_app", "only GET and HEAD reach /healthz", d)
    st, _, d, _ = s.app("notes", "/healthz")
    check(b"uptime_seconds" not in d, "/healthz on an app's host goes to the app, not the server", d)

    # GET /metrics: admin listener only, behind the token, Host and Origin checks
    st, _, d, _ = call(s.adm, "GET", "/metrics", adm_host)
    check(st == 401 and kind(d) == "unauthorized" and b"spared_" not in d, "/metrics without the token is 401", d)
    st, _, d, _ = s.admin("GET", "/metrics", {"Origin": "http://evil.example"})
    check(st == 403 and kind(d) == "origin_refused", "/metrics refuses an Origin header", d)
    st, _, d, _ = s.admin("GET", "/metrics", host="attacker.example")
    check(st == 403 and kind(d) == "bad_host", "/metrics refuses a foreign Host", d)
    for path, host in [("/metrics", pub_host), ("/metrics", f"notes.localhost:{s.pub}"), ("/_/metrics", pub_host)]:
        st, _, d, _ = call(s.pub, "GET", path, host, s.auth)
        check(st == 404 and b"spared_requests_total" not in d, f"the public listener has no metrics at {path} (Host {host.split(':')[0]}), even with the token", d[:120])
    st, h, d, _ = s.admin("GET", "/metrics")
    text = d.decode()
    metric = lambda line: next((float(l.rsplit(" ", 1)[1]) for l in text.splitlines() if l.startswith(line + " ")), None)
    check(st == 200 and h.get("content-type", "").startswith("text/plain; version=0.0.4"), "/metrics with the token is 200, Prometheus text format", h)
    check(all(l.startswith("# ") or re.fullmatch(r'[a-z_]+(\{[a-z_]+="[^"]*"(,[a-z_]+="[^"]*")*\})? [0-9.e+]+', l) for l in text.splitlines()),
          "every metrics line is a comment or `name{labels} value`")
    check(metric('spared_requests_total{listener="public",status="2xx"}') > 10
          and metric('spared_requests_total{listener="admin",status="4xx"}') >= 3, "metrics count requests by listener and status class")
    check(metric('spared_errors_total{listener="public",kind="app_trap"}') >= 1
          and metric('spared_errors_total{listener="public",kind="cpu_limit"}') >= 1
          and metric('spared_errors_total{listener="admin",kind="unauthorized"}') >= 2, "metrics count errors by kind")
    check(metric("spared_wakes_total") >= 5 and metric('spared_evictions_total{reason="error"}') >= 2
          and metric('spared_evictions_total{reason="deploy"}') >= 1 and metric('spared_evictions_total{reason="admin"}') >= 1,
          "metrics count wakes and evictions by reason")
    check(metric('spared_apps{state="deployed"}') == len(names) and metric('spared_apps{state="hot"}') is not None
          and metric("spared_components_loaded") >= 1 and metric("process_resident_memory_bytes") > 1 << 20
          and metric("spared_uptime_seconds") is not None and 'spared_build_info{version="' in text
          and metric("spared_prefetch_bytes_total") is not None,
          "metrics report apps deployed and hot, components, memory, uptime, the version and bytes read ahead")
    check(not any(n.decode() in text for n in names) and s.token not in text and s.data not in text,
          "metrics carry no app name, token or path")

    # the access log: one JSON line per request with the documented fields, and never a query
    # string, a header or a body
    marker = f"{random.randrange(10**12):012d}"
    st, h, _, _ = s.app("notes", f"/api/notes?token=QUERYSECRET{marker}", method="POST",
                        body=json.dumps({"text": f"BODYSECRET{marker}"}),
                        headers={"content-type": "application/json", "X-Api-Key": f"HEADERSECRET{marker}", "Cookie": f"session=COOKIESECRET{marker}"})
    rid = h.get("x-spared-request-id")
    st2, h2, _, _ = s.app("hostile", f"/panic?why=QUERYSECRET{marker}")
    st3, h3, _, _ = s.admin("GET", f"/v1/apps/notes/logs?lines=1&x=QUERYSECRET{marker}")
    time.sleep(0.3)
    log = open(logf, errors="replace").read()
    lines = {l["request_id"]: l for l in access_lines(log)}
    mine = lines.get(rid, {})
    check(st == 201 and set(mine) == ACCESS_FIELDS and (mine["listener"], mine["app"], mine["method"], mine["path"], mine["status"]) == ("public", "notes", "POST", "/api/notes", 201),
          "a request writes one access-log line: time, listener, app, method, path, status, duration, request id", mine)
    check(re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z", mine["time"]) and 0 < mine["duration_ms"] < 30000,
          "the access-log time is UTC RFC 3339 and the duration is in milliseconds", mine)
    trap = lines.get(h2.get("x-spared-request-id"), {})
    check(st2 == 500 and (trap.get("app"), trap.get("status"), trap.get("error"), trap.get("path")) == ("hostile", 500, "app_trap", "/panic"),
          "an error made by the server is logged with its kind", trap)
    adm = lines.get(h3.get("x-spared-request-id"), {})
    check(st3 == 200 and (adm.get("listener"), adm.get("app"), adm.get("path")) == ("admin", "notes", "/v1/apps/notes/logs"),
          "admin requests are logged too, without their query string", adm)
    check(marker not in log, "no query string, header, cookie or body reaches the server's output")
    check(s.token not in log, "the admin token is never printed")
    check(re.search(r"^spared \d+\.\d+\.\d+\S* (\([0-9a-f]+\) )?starting$", log, re.M) and "data dir: " in log
          and f"apps:  http://127.0.0.1:{s.pub}" in log and "health check: GET /healthz" in log
          and f"admin: http://127.0.0.1:{s.adm}" in log and "access log: on" in log,
          "startup prints the version, the data dir, both listen addresses and the access-log state")
    rc, out, err = spared(s, "--version")
    check(rc == 0 and re.fullmatch(r"spared \d+\.\d+\.\d+\S*( \([0-9a-f]{7,12}\))?\n", out) and out.split()[1] == v["version"],
          "`spared --version` prints the version /healthz reports", out)
    rc, out, err = spared(s, "--help")
    check(rc == 0 and "deploy" in out and not any(w in out for w in ("bench-3b", "run-probe", "escape-test", "bench-wake", "bench-density", "compile")),
          "`spared --help` lists user commands and no lab commands", out)
    for lab in ("bench-3b", "run-probe", "escape-test", "bench-wake", "bench-density", "compile"):
        rc, out, err = spared(s, lab, "--help")
        check(rc != 0 and "unrecognized subcommand" in err, f"the release binary has no `{lab}` command", err)

    # `spared logs` on an app that wrote nothing says so
    st, v = s.deploy("quiet", os.path.join(DIST, "notes/app.wasm"), 'name = "quiet"\n[grants]\ndata = true\n')
    assert st == 200, v
    rc, out, err = spared(s, "logs", "quiet")
    check(rc == 0 and out == "" and "no output yet" in err and "quiet" in err, "`spared logs` on an empty log prints a 'no output yet' line", (out, err))
    rc, out, err = spared(s, "--json", "logs", "quiet")
    j = json.loads(out)
    check(rc == 0 and j["lines"] == [] and "note" in j, "`spared logs --json` on an empty log has no lines and a note", out)
    st, _, _, _ = s.admin("DELETE", "/v1/apps/quiet")
    assert st == 200


def jadmin(s, method, path, payload=None, **kw):
    body = None if payload is None else json.dumps(payload)
    st, _, d, _ = s.admin(method, path, {"content-type": "application/json"} if body else None, body, **kw)
    try:
        return st, json.loads(d)
    except ValueError:
        return st, d


def spared(s, *args, check_rc=None):
    env = dict(os.environ, HOME=s.home, SPARED_TOKEN=s.token, SPARED_URL=f"http://127.0.0.1:{s.adm}")
    r = subprocess.run([BIN, *args], env=env, capture_output=True, text=True, timeout=120)
    return r.returncode, r.stdout, r.stderr


ID_RE = re.compile(r"^\d{8}T\d{6}Z$")


def backups(s, notes_manifest):
    wasm = os.path.join(DIST, "notes/app.wasm")
    bk_root = os.path.join(s.data, "backups")
    ctype = {"content-type": "application/json"}

    def deploy(name):
        st, v = s.deploy(name, wasm, notes_manifest.replace('name = "notes"', f'name = "{name}"'))
        assert st == 200, v

    def post(app, text):
        st, _, d, _ = s.app(app, "/api/notes", "POST", json.dumps({"text": text}), ctype)
        return st

    def texts(app):
        st, _, d, _ = s.app(app, "/api/notes")
        assert st == 200, (st, d)
        return [n["text"] for n in json.loads(d)]

    deploy("bk")

    # the new endpoints are guarded like the others
    for method, path in [("POST", "/v1/apps/bk/backup"), ("GET", "/v1/apps/bk/backups"), ("POST", "/v1/apps/bk/evict"), ("POST", "/v1/apps/bk/restore")]:
        st, _, d, _ = call(s.adm, method, path, f"127.0.0.1:{s.adm}")
        check(st == 401 and kind(d) == "unauthorized", f"{method} {path} needs the token", d)
        st, _, d, _ = s.admin(method, path, {"Origin": "http://x"})
        check(st == 403 and kind(d) == "origin_refused", f"{method} {path} refuses an Origin header", d)
        st, _, d, _ = s.admin(method, path, host="attacker.example")
        check(st == 403 and kind(d) == "bad_host", f"{method} {path} refuses a foreign Host", d)
    for method, path in [("POST", "/v1/apps/nope/backup"), ("GET", "/v1/apps/nope/backups"), ("POST", "/v1/apps/nope/evict"), ("POST", "/v1/apps/nope/restore")]:
        st, v = jadmin(s, method, path, {"backup_id": "20261110T040506Z"} if "restore" in path else None)
        check(st == 404 and v["error"]["kind"] == "no_such_app", f"{method} {path} on an unknown app is no_such_app", v)
    st, v = jadmin(s, "POST", "/v1/apps/Bad_Name/backup")
    check(st == 400 and v["error"]["kind"] == "bad_name", "backup of an invalid name is bad_name", v)

    # evict drops the live instance; the next request wakes a fresh one
    check(post("bk", "one") == 201, "POST a note to bk")
    before = json.loads(s.admin("GET", "/v1/apps/bk")[2])
    check(before["state"] == "hot", "an app that just served is hot", before)
    st, v = jadmin(s, "POST", "/v1/apps/bk/evict")
    check(st == 200 and v["state"] == "idle" and v["name"] == "bk", "POST evict returns the idle state", v)
    check(texts("bk") == ["one"], "evict then request wakes the app with its data")
    after = json.loads(s.admin("GET", "/v1/apps/bk")[2])
    check(after["wakes"] == before["wakes"] + 1 and after["state"] == "hot", "the request after an evict was a new wake", (before["wakes"], after["wakes"]))

    # backup, mutate, restore
    st, b1 = jadmin(s, "POST", "/v1/apps/bk/backup")
    check(st == 200 and ID_RE.match(b1["backup_id"]) and b1["name"] == "bk" and b1["bytes"] > 0 and b1["files"] >= 1, "POST backup returns id, bytes and files", b1)
    check(re.match(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\dZ$", b1["created_at"]) and b1["integrity"] == INTEGRITY and b1["journal_clean"] is True, "backup reports created_at, a hash-verified copy and a clean journal", b1)
    check("app data only" in b1["contains"] and "not its code" in b1["contains"], "backup says it holds data only, not code", b1["contains"])
    check(json.loads(s.admin("GET", "/v1/apps/bk")[2])["state"] == "idle", "an app is idle after a backup (instance dropped)")
    bdir = os.path.join(bk_root, "bk", b1["backup_id"])
    check(os.path.isfile(os.path.join(bdir, "app.db")), "the backup directory holds app.db")
    check(sha256_file(os.path.join(bdir, "app.db")) == sha256_file(os.path.join(s.data, "apps/bk/data/app.db")), "the backed-up app.db is byte-identical to the app's")
    check(mode(bk_root) == "0o700" and mode(os.path.join(bk_root, "bk")) == "0o700" and mode(bdir) == "0o700" and mode(os.path.join(bdir, "app.db")) == "0o600",
          "backups/, backups/<app>/, the backup and its files are 0700 / 0600")
    check(not [f for f in os.listdir(os.path.join(bk_root, "bk")) if f.startswith(".tmp")], "no temporary copy is left behind")
    check(post("bk", "two") == 201 and texts("bk") == ["one", "two"], "mutate after the backup")
    st, v = jadmin(s, "POST", "/v1/apps/bk/restore", {"backup_id": b1["backup_id"]})
    pre = v.get("pre_restore_id", "")
    check(st == 200 and v["restored_from"] == b1["backup_id"] and pre.startswith("pre-restore-") and ID_RE.match(pre[len("pre-restore-"):]), "POST restore reports the pre-restore copy", v)
    check(texts("bk") == ["one"], "restore returns the old data")
    check(os.path.isdir(os.path.join(bk_root, "bk", pre)), "the replaced data sits in backups/<app>/pre-restore-<ts>/")
    st, v = jadmin(s, "POST", "/v1/apps/bk/restore", {"backup_id": pre})
    check(st == 200 and texts("bk") == ["one", "two"], "restoring the pre-restore copy undoes the restore", v)
    check(json.loads(s.admin("GET", "/v1/apps/bk")[2])["version"] == 1, "a restore leaves the code version alone")

    # bad ids and bodies
    st, v = jadmin(s, "POST", "/v1/apps/bk/restore", {"backup_id": "20990101T000000Z"})
    check(st == 404 and v["error"]["kind"] == "no_such_backup", "restoring an unknown id is no_such_backup", v)
    for bad in ["..", "../x", "%2e%2e", "../../apps/bk", "20261110T040506Z/..", "pre-restore-../x", "", "latest"]:
        st, v = jadmin(s, "POST", "/v1/apps/bk/restore", {"backup_id": bad})
        check(st == 400 and v["error"]["kind"] == "bad_backup_id", f"restore id {bad!r} is refused", v)
    for bad in ["..", "%2e%2e", "latest", "20261110T040506Z%2f..", "pre-restore-%2e%2e"]:
        st, v = jadmin(s, "DELETE", f"/v1/apps/bk/backups/{bad}")
        check(st == 400 and v["error"]["kind"] == "bad_backup_id", f"DELETE backups/{bad} is refused", v)
    for bad in ["../..", "../../bk", "x/y"]:
        st, v = jadmin(s, "DELETE", f"/v1/apps/bk/backups/{bad}")
        check(st == 404 and v["error"]["kind"] in ("no_route", "no_such_backup"), f"DELETE backups/{bad} reaches nothing", v)
    st, v = jadmin(s, "POST", "/v1/apps/bk/restore", "not json")
    check(st == 400 and v["error"]["kind"] == "bad_request", "restore with a non-JSON body is bad_request", v)
    check(os.path.isfile(os.path.join(s.data, "apps/bk/data/app.db")) and os.path.isdir(bdir) and texts("bk") == ["one", "two"], "refused requests changed nothing")

    # listing, newest first
    ids = [b1["backup_id"]]
    for _ in range(2):
        st, b = jadmin(s, "POST", "/v1/apps/bk/backup")
        assert st == 200, b
        ids.append(b["backup_id"])
    check(ids == sorted(ids) and len(set(ids)) == 3, "three backups in a row get distinct, increasing ids", ids)
    st, v = jadmin(s, "GET", "/v1/apps/bk/backups")
    plain = [b["backup_id"] for b in v["backups"] if b["kind"] == "backup"]
    check(st == 200 and plain == ids[::-1], "backups are listed newest first", v)
    stamp = lambda b: b["backup_id"].replace("pre-restore-", "")
    check([stamp(b) for b in v["backups"]] == sorted((stamp(b) for b in v["backups"]), reverse=True), "pre-restore copies sort in with the backups by time")
    check(any(b["kind"] == "pre-restore" for b in v["backups"]) and v["max_backups"] == 4 and all(b["bytes"] > 0 and b["files"] >= 1 for b in v["backups"]), "the list shows kind, bytes, files and the retention limit", v)
    st, v = jadmin(s, "DELETE", f"/v1/apps/bk/backups/{ids[1]}")
    check(st == 200 and not os.path.exists(os.path.join(bk_root, "bk", ids[1])), "DELETE a backup removes it", v)
    st, v = jadmin(s, "DELETE", f"/v1/apps/bk/backups/{ids[1]}")
    check(st == 404 and v["error"]["kind"] == "no_such_backup", "deleting it again is no_such_backup", v)

    # retention: --max-backups-per-app 4
    deploy("bk2")
    made = []
    for i in range(6):
        post("bk2", f"n{i}")
        st, b = jadmin(s, "POST", "/v1/apps/bk2/backup")
        assert st == 200, b
        made.append(b)
    st, v = jadmin(s, "GET", "/v1/apps/bk2/backups")
    got = [b["backup_id"] for b in v["backups"]]
    check(got == [b["backup_id"] for b in made[2:]][::-1], "retention keeps the newest 4 of 6 backups", got)
    check(made[4].get("removed_old") == [made[0]["backup_id"]] and "removed_old" not in made[3], "the backup that trimmed one names it in removed_old", made[4])
    check(sorted(d for d in os.listdir(os.path.join(bk_root, "bk2"))) == sorted(got), "trimmed backups are gone from disk")

    # an app's backups outlive `spared rm`, and the response says so
    st, v = jadmin(s, "DELETE", "/v1/apps/bk2")
    check(st == 200 and v.get("backups_kept") == 4 and len(os.listdir(os.path.join(bk_root, "bk2"))) == 4, "rm keeps the backups and reports how many", v)

    # a non-empty journal is copied with the database and reported
    deploy("bk3")
    post("bk3", "x")
    jadmin(s, "POST", "/v1/apps/bk3/evict")
    with open(os.path.join(s.data, "apps/bk3/data/app.db-journal"), "wb") as f:
        f.write(b"\0" * 32)
    st, v = jadmin(s, "POST", "/v1/apps/bk3/backup")
    d3 = os.path.join(bk_root, "bk3", v.get("backup_id", "x"))
    check(st == 200 and v["journal_clean"] is False and v["integrity"] == INTEGRITY and "journal" in v["note"] and os.path.isfile(os.path.join(d3, "app.db-journal")),
          "a non-empty journal is copied with the database and the response says so", v)
    s.admin("DELETE", "/v1/apps/bk3")

    hostile_database(s, deploy, bk_root)

    backup_under_load(s, deploy, post, texts, bk_root)
    cli_backups(s, post, texts)


def hostile_database(s, deploy, bk_root):
    # H1: an app.db whose index expression makes SQLite allocate 150 MB per row when the index is
    # checked. The host must copy it as bytes and never open it (it used to take 9 s and 734 MB).
    import sqlite3
    deploy("evildb")
    db = os.path.join(s.data, "apps/evildb/data/app.db")
    c = sqlite3.connect(db)
    c.execute("CREATE TABLE t(x)")
    c.executemany("INSERT INTO t VALUES (?)", [(i,) for i in range(12)])
    c.execute("CREATE INDEX i ON t(x+0)")
    c.commit()
    c.execute("PRAGMA writable_schema=ON")
    c.execute("UPDATE sqlite_master SET sql='CREATE INDEX i ON t(length(hex(zeroblob(150000000)))*0+x+0)' WHERE name='i'")
    c.commit()
    c.close()
    want = sha256_file(db)
    t = time.monotonic()
    st, v = jadmin(s, "POST", "/v1/apps/evildb/backup")
    took = time.monotonic() - t
    check(st == 200 and v["integrity"] == INTEGRITY and took < 2, f"a backup of a hostile app.db is a verified byte copy in {took:.2f}s, without opening it", v)
    check(sha256_file(os.path.join(bk_root, "evildb", v["backup_id"], "app.db")) == want, "the hostile app.db is copied byte for byte")
    t = time.monotonic()
    st, r = jadmin(s, "POST", "/v1/apps/evildb/restore", {"backup_id": v["backup_id"]})
    took = time.monotonic() - t
    check(st == 200 and r["integrity"] == INTEGRITY and took < 2 and sha256_file(db) == want, f"restoring it is a verified copy too ({took:.2f}s)", r)
    s.admin("DELETE", "/v1/apps/evildb")


def backup_under_load(s, deploy, post, texts, bk_root):
    deploy("bkload")
    stop = threading.Event()
    acked = []
    failures = []

    def writer():
        i = 0
        while not stop.is_set():
            st = post("bkload", f"w{i}")
            if st != 201:
                failures.append((i, st))
                return
            acked.append(i)
            i += 1

    th = threading.Thread(target=writer)
    th.start()
    time.sleep(0.3)
    taken = []
    for _ in range(3):
        floor = len(acked)
        st, b = jadmin(s, "POST", "/v1/apps/bkload/backup")
        assert st == 200, b
        taken.append((b, floor))
        time.sleep(0.3)
    stop.set()
    th.join()
    check(not failures and len(acked) > 50, f"{len(acked)} writes were acknowledged while 3 backups ran, none failed", failures)
    check(all(b["integrity"] == INTEGRITY and b["journal_clean"] for b, _ in taken), "every backup taken under write load is a verified copy of a clean journal", [b for b, _ in taken])
    for b, floor in taken:
        st, v = jadmin(s, "POST", "/v1/apps/bkload/restore", {"backup_id": b["backup_id"]})
        assert st == 200, v
        got = texts("bkload")
        check(got == [f"w{i}" for i in range(len(got))] and len(got) >= floor,
              f"restoring {b['backup_id']} gives a clean prefix holding all {floor} writes acknowledged before it ({len(got)} rows)", (len(got), floor))
    st, _, d, _ = s.app("bkload", "/api/notes", "POST", json.dumps({"text": "after"}), {"content-type": "application/json"})
    check(st == 201, "the restored app accepts writes")
    s.admin("DELETE", "/v1/apps/bkload")


def cli_backups(s, post, texts):
    rc, out, err = spared(s, "--json", "backup", "bk")
    v = json.loads(out)
    check(rc == 0 and ID_RE.match(v["backup_id"]) and v["integrity"] == INTEGRITY, "spared backup --json prints the backup", out)
    rc, out, _ = spared(s, "backup", "bk")
    check(rc == 0 and "backed up bk as" in out and "data only" in out and "spared restore bk --from" in out, "spared backup says what it holds and how to restore", out)
    rc, out, _ = spared(s, "--json", "backups", "bk")
    v2 = json.loads(out)
    check(rc == 0 and v2["backups"][0]["backup_id"] > v["backup_id"] and len(v2["backups"]) <= 8, "spared backups --json lists newest first", out)
    rc, out, _ = spared(s, "backups", "bk")
    check(rc == 0 and "ID" in out and v["backup_id"] in out, "spared backups prints a table", out)
    post("bk", "three")
    rc, out, _ = spared(s, "--json", "restore", "bk", "--from", v["backup_id"])
    e = json.loads(out)["error"]
    check(rc == 1 and e["kind"] == "confirm_required" and "--yes" in e["fix"] and "undone" in e["fix"], "spared restore without --yes refuses and names the fix", out)
    check("three" in texts("bk"), "the refused restore changed nothing")
    rc, out, _ = spared(s, "restore", "bk", "--from", v["backup_id"])
    check(rc == 1, "spared restore without --yes exits 1 in text mode")
    rc, out, _ = spared(s, "--json", "restore", "bk", "--from", v["backup_id"], "--yes")
    r = json.loads(out)
    check(rc == 0 and r["restored_from"] == v["backup_id"] and "three" not in texts("bk") and r["undo"].startswith("spared restore bk --from pre-restore-"), "spared restore --yes restores and names the undo", out)
    rc, out, _ = spared(s, "--json", "restore", "bk", "--from", "../etc", "--yes")
    check(rc == 1 and json.loads(out)["error"]["kind"] == "bad_backup_id", "spared restore with a traversal id is refused", out)
    rc, out, _ = spared(s, "--json", "restore", "nope", "--from", v["backup_id"], "--yes")
    check(rc == 1 and json.loads(out)["error"]["kind"] == "no_such_app" and json.loads(out)["error"]["fix"], "spared restore on an unknown app names the fix", out)
    rc, out, _ = spared(s, "--json", "evict", "bk")
    check(rc == 0 and json.loads(out)["state"] == "idle", "spared evict --json returns the idle state", out)
    rc, out, _ = spared(s, "evict", "bk")
    check(rc == 0 and "evicted bk" in out, "spared evict prints a line")

    rc, out, err = spared(s, "--json", "bench", "bk", "--requests", "300", "--path", "/api/notes")
    b = json.loads(out)
    shape = {"app", "path", "status", "unit", "measure", "cold", "warm"}
    check(rc == 0 and set(b) == shape and b["app"] == "bk" and b["path"] == "/api/notes" and b["status"] == 200 and b["unit"] == "ms", "spared bench --json has the stable top-level shape", out)
    for part, n in [("cold", 20), ("warm", 300)]:
        x = b[part]
        check(set(x) == {"samples", "p50_ms", "p99_ms", "max_ms"} and x["samples"] == n and 0 < x["p50_ms"] <= x["p99_ms"] <= x["max_ms"] and all(isinstance(x[k], float) for k in ("p50_ms", "p99_ms", "max_ms")),
              f"bench {part}: {n} samples, 0 < p50 <= p99 <= max", x)
    check(b["cold"]["p50_ms"] > b["warm"]["p50_ms"], f"cold wake p50 {b['cold']['p50_ms']} ms is above warm p50 {b['warm']['p50_ms']} ms")
    check(json.loads(s.admin("GET", "/v1/apps/bk")[2])["wakes"] >= 20, "bench evicted the app for each cold cycle")
    rc, out, _ = spared(s, "bench", "bk", "--requests", "50")
    check(rc == 0 and "cold wake" in out and "p99" in out, "spared bench prints a table", out)
    rc, out, _ = spared(s, "--json", "bench", "bk", "--path", "/nope", "--requests", "5")
    check(rc == 1 and json.loads(out)["error"]["kind"] == "bad_status" and "--path" in json.loads(out)["error"]["fix"], "bench of a 404 path names the fix", out)
    rc, out, _ = spared(s, "--json", "bench", "nope")
    check(rc == 1 and json.loads(out)["error"]["kind"] == "no_such_app", "bench of an unknown app is no_such_app", out)


def bad_backup_ids(s):
    """A backup id is checked by bytes: one with a multi-byte character where the id format has a
    digit is a 400 bad_backup_id, never a dropped connection."""
    for bad in ["1234567\u00e9" + "1234567", "pre-restore-1234567\u00e9" + "1234567", "\u00e9" * 8, "2026111\u0663T040506Z"]:
        body = json.dumps({"backup_id": bad}, ensure_ascii=False).encode()
        try:
            st, _, d, _ = s.admin("POST", "/v1/apps/notes/restore", {"content-type": "application/json"}, body)
        except OSError as e:
            raise SystemExit(f"FAIL - restore with backup_id {bad!r} got no response ({e})")
        check(st == 400 and kind(d) == "bad_backup_id" and "spared backups" in json.loads(d)["error"]["fix"],
              f"restore with the non-ASCII backup_id {bad!r} is 400 bad_backup_id", (st, d[:200]))
    for bad in ["1234567%C3%A91234567", "%C3%A9" * 8]:
        st, _, d, _ = s.admin("DELETE", f"/v1/apps/notes/backups/{bad}")
        check(st == 400 and kind(d) == "bad_backup_id", f"deleting backup {bad} is 400 bad_backup_id", (st, d[:200]))
    st, _, d, _ = s.admin("GET", "/v1/apps/notes/backups")
    check(st == 200 and s.p.poll() is None, "the server is unharmed and still lists backups")


def slow_clients(s):
    # 40 connections each send 7 MB of an 8 MB body, then stall. The server holds at most
    # --max-body-buffer-mb of such bodies (64 MB by default, so eight): it refuses the rest with
    # 503 as soon as their headers are in, and cuts off the eight it holds after the body timeout.
    socks = []
    chunk = b"x" * (1 << 20)
    for _ in range(40):
        c = socket.create_connection(("127.0.0.1", s.pub))
        try:
            c.sendall(f"POST /api/notes HTTP/1.1\r\nHost: notes.localhost\r\nContent-Length: {8 << 20}\r\n\r\n".encode())
            for _ in range(7):
                c.sendall(chunk)
        except OSError:
            pass
        socks.append(c)
    t0 = time.monotonic()
    answered = cut = refused = 0
    for c in socks:
        c.settimeout(max(0.1, CLIENT_TIMEOUT + 3 - (time.monotonic() - t0)))
        try:
            data = c.recv(4096)
            if data.startswith(b"HTTP/1.1 503"):
                refused += 1
            if data == b"" or data.startswith(b"HTTP/1.1 408"):
                cut += 1
            if data == b"" or data.startswith((b"HTTP/1.1 408", b"HTTP/1.1 503")):
                answered += 1
        except ConnectionResetError:
            refused += 1
            answered += 1
        except OSError:
            pass
        c.close()
    took = time.monotonic() - t0
    check(answered == 40 and took < CLIENT_TIMEOUT + 3, f"40 stalled 8 MB uploads are refused or cut off after the body timeout ({took:.1f}s)", answered)
    check(refused >= 20 and cut >= 1, f"the server refuses what its body budget cannot hold and cuts off the rest ({refused} refused, {cut} cut off)")

    # one header line every 0.5 s never completes
    c = socket.create_connection(("127.0.0.1", s.pub))
    c.sendall(b"GET / HTTP/1.1\r\nHost: notes.localhost\r\n")
    t0 = time.monotonic()
    closed = False
    while time.monotonic() - t0 < CLIENT_TIMEOUT + 4:
        try:
            c.sendall(b"X-Slow: 1\r\n")
        except OSError:
            closed = True
            break
        c.settimeout(0.5)
        try:
            if c.recv(4096) == b"":
                closed = True
                break
        except socket.timeout:
            pass
        except OSError:
            closed = True
            break
    took = time.monotonic() - t0
    c.close()
    check(closed and took < CLIENT_TIMEOUT + 2, f"a trickled header is cut off after the header timeout ({took:.1f}s)")

    # an idle keep-alive connection is closed too
    c = socket.create_connection(("127.0.0.1", s.pub))
    c.sendall(b"GET / HTTP/1.1\r\nHost: notes.localhost\r\n\r\n")
    c.settimeout(CLIENT_TIMEOUT + 4)
    t0 = time.monotonic()
    got = b""
    try:
        while True:
            d = c.recv(65536)
            if not d:
                break
            got += d
    except OSError:
        pass
    took = time.monotonic() - t0
    c.close()
    check(got.startswith(b"HTTP/1.1 200") and took < CLIENT_TIMEOUT + 2, f"an idle keep-alive connection is closed ({took:.1f}s)")


def disconnects(s):
    # H2: a client that goes away mid-request must not break the instance for the next client.
    for i in range(3):
        c = socket.create_connection(("127.0.0.1", s.pub))
        c.sendall(b"GET /spin HTTP/1.1\r\nHost: hostile.localhost\r\n\r\n")
        time.sleep(0.1)
        c.close()
        st, _, d, _ = s.app("hostile", "/")
        check(st == 200 and d == b"ok\n", f"after a client abandons a running request, the next request is 200 ({i + 1}/3)", (st, d[:200]))
    stop, bad, n = threading.Event(), [], [0]

    def getter():
        while not stop.is_set():
            st, _, d, _ = s.app("notes", "/api/notes")
            n[0] += 1
            if st != 200:
                bad.append((st, d[:200]))

    th = threading.Thread(target=getter)
    th.start()
    rng = random.Random(7)
    for i in range(200):
        body = json.dumps({"text": f"abandoned {i}"}).encode()
        c = socket.create_connection(("127.0.0.1", s.pub))
        c.sendall(b"POST /api/notes HTTP/1.1\r\nHost: notes.localhost\r\nContent-Type: application/json\r\n"
                  + f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
        time.sleep(rng.random() * 0.004)
        c.close()
    time.sleep(0.2)
    stop.set()
    th.join()
    check(not bad and n[0] >= 50, f"200 abandoned POSTs interleaved with {n[0]} GETs from another client: every GET is 200", bad[:3])
    st, _, d, _ = s.app("notes", "/api/notes")
    texts = [x["text"] for x in json.loads(d)]
    check(st == 200 and all(texts.count(f"abandoned {i}") <= 1 for i in range(200)), f"abandoned writes committed whole or not at all ({sum(t.startswith('abandoned') for t in texts)} of 200 landed)")


def never_read(s):
    # M1: clients that request a big response and never read it are cut off by the write timeout.
    socks = []
    for _ in range(8):
        c = socket.socket()
        c.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
        c.connect(("127.0.0.1", s.pub))
        c.sendall(b"GET /big?mb=14 HTTP/1.1\r\nHost: hostile.localhost\r\n\r\n")
        socks.append(c)
    time.sleep(CLIENT_TIMEOUT + 4)
    st, _, _, t = s.app("notes", "/api/notes")
    check(st == 200 and t < 1, f"other clients are served while 8 clients refuse to read ({t * 1000:.0f} ms)")
    cut = 0
    for c in socks:
        got, closed = 0, False
        c.settimeout(2)
        try:
            while True:
                d = c.recv(1 << 20)
                if not d:
                    closed = True
                    break
                got += len(d)
        except ConnectionResetError:
            closed = True
        except OSError:
            pass
        c.close()
        if closed and got < 14 << 20:
            cut += 1
    check(cut == 8, f"8 clients that never read a 14 MB response are disconnected after the write timeout ({cut} of 8)")
    st, _, d, _ = s.app("hostile", "/")
    check(st == 200, "the app answers after its non-reading clients were cut off")


def cpu_seconds(pid):
    with open(f"/proc/{pid}/stat") as f:
        rest = f.read().rsplit(")", 1)[1].split()
    return (int(rest[11]) + int(rest[12])) / os.sysconf("SC_CLK_TCK")


def abort_admin(s, method, path, body=b"", headers=None, after=0.3):
    """Send an admin request and close the connection `after` seconds later without reading the
    answer (Ctrl-C in the CLI, a proxy timeout, the CLI's own 120 s timeout)."""
    h = {"Host": f"127.0.0.1:{s.adm}", **s.auth, "Content-Length": str(len(body)), **(headers or {})}
    head = f"{method} {path} HTTP/1.1\r\n" + "".join(f"{k}: {v}\r\n" for k, v in h.items()) + "\r\n"
    c = socket.create_connection(("127.0.0.1", s.adm))
    c.sendall(head.encode() + body)
    time.sleep(after)
    c.close()


def admin_within(s, method, path, what, secs=5):
    try:
        return call(s.adm, method, path, f"127.0.0.1:{s.adm}", s.auth, timeout=secs)
    except OSError as e:
        raise SystemExit(f"FAIL - {what}: no answer within {secs} s ({e})")


def admin_aborts(s):
    """An admin call whose client goes away must still run to completion, with the app's instance
    lock held throughout: no half-made backup, no restore under a live instance, no half-deleted
    app, no redeploy that leaves the app answering 404."""
    wasm = os.path.join(DIST, "notes/app.wasm")
    manifest = open(os.path.join(DIST, "notes/spared.toml")).read()

    def deploy(name):
        st, v = s.deploy(name, wasm, manifest.replace('name = "notes"', f'name = "{name}"'))
        assert st == 200, v

    def sparse(path, mb):
        with open(path, "wb") as f:
            f.truncate(mb << 20)

    # backup: the lock is held until the copy is whole
    deploy("abk")
    data, bk = os.path.join(s.data, "apps/abk/data"), os.path.join(s.data, "backups/abk")
    sparse(os.path.join(data, "big.bin"), 500)
    st, _, _, _ = s.app("abk", "/api/notes")
    assert st == 200
    abort_admin(s, "POST", "/v1/apps/abk/backup", after=0.3)
    in_flight = any(f.startswith(".tmp-") for f in os.listdir(bk))
    st, _, d, t = s.app("abk", "/api/notes")
    partial = [f for f in os.listdir(bk) if f.startswith(".tmp-")]
    whole = [f for f in os.listdir(bk) if ID_RE.match(f)]
    check(in_flight and st == 200 and not partial and len(whole) == 1,
          f"a backup whose client went away finishes before the app serves again ({t:.1f}s wait)", (in_flight, st, partial, whole))
    for _ in range(200):
        whole = [f for f in os.listdir(bk) if ID_RE.match(f)]
        if whole:
            break
        time.sleep(0.1)
    bid = whole[0]

    # restore: no write lands while the data directory is swapped
    abort_admin(s, "POST", "/v1/apps/abk/restore", json.dumps({"backup_id": bid}).encode(), {"content-type": "application/json"}, after=0.3)
    in_flight = any(f.startswith("data.restore-") for f in os.listdir(os.path.join(s.data, "apps/abk")))
    st, _, d, t = s.app("abk", "/api/notes", "POST", json.dumps({"text": "written during a restore"}), {"content-type": "application/json"})
    leftovers = [f for f in os.listdir(os.path.join(s.data, "apps/abk")) if f.startswith("data.restore-")]
    pre = [f for f in os.listdir(bk) if f.startswith("pre-restore-")]
    s.admin("POST", "/v1/apps/abk/evict")
    st2, _, d2, _ = s.app("abk", "/api/notes")
    check(in_flight and st == 201 and not leftovers and len(pre) == 1 and b"written during a restore" in d2,
          f"a restore whose client went away finishes before the next write, and that write is in the live data ({t:.1f}s wait)", (in_flight, st, leftovers, pre, d2[:200]))
    s.admin("DELETE", "/v1/apps/abk")
    shutil.rmtree(bk, ignore_errors=True)

    # delete: the app is removed whole, not left in the table's absence with its files on disk
    st, v = s.deploy("abr", HOSTILE, hostile_manifest("abr", cpu_ms=1000, deadline_ms=10000))
    assert st == 200, v
    spin = threading.Thread(target=lambda: s.app("abr", "/spin"))
    spin.start()
    time.sleep(0.3)
    abort_admin(s, "DELETE", "/v1/apps/abr", after=0.2)
    spin.join()
    gone = False
    for _ in range(40):
        if not os.path.exists(os.path.join(s.data, "apps/abr")):
            gone = True
            break
        time.sleep(0.1)
    st, _, _, _ = s.admin("GET", "/v1/apps/abr")
    check(gone and st == 404, "a delete whose client went away still removes the app's directory", (gone, st))

    # redeploy: the app keeps answering, and evict neither hangs nor spins
    deploy("abd")
    d = os.path.join(s.data, "apps/abd/data/many")
    os.makedirs(d)
    for i in range(150000):
        os.close(os.open(os.path.join(d, f"f{i}"), os.O_WRONLY | os.O_CREAT, 0o600))
    mhdr = base64.b64encode(manifest.replace('name = "notes"', 'name = "abd"').encode()).decode()
    body = open(wasm, "rb").read()
    answers = []
    for delay in [0.0, 0.05, 0.1, 0.2, 0.4, 0.7]:
        abort_admin(s, "PUT", "/v1/apps/abd", body, {"x-spared-manifest": mhdr}, after=delay)
        st, _, _, _ = s.app("abd", "/api/notes", timeout=10)
        st2, _, _, _ = admin_within(s, "POST", "/v1/apps/abd/evict", f"evict after a redeploy aborted at {delay} s")
        answers.append((delay, st, st2))
    check(all(a[1] == 200 and a[2] == 200 for a in answers),
          "after a redeploy whose client went away at 0 to 700 ms, the app answers and evict answers", answers)
    time.sleep(0.5)
    cpu0 = cpu_seconds(s.p.pid)
    time.sleep(1)
    idle = cpu_seconds(s.p.pid) - cpu0
    check(idle < 0.2, f"the server is idle after the aborted redeploys ({idle:.2f} CPU-s in 1 s)", idle)
    st, v = s.deploy("abd", wasm, manifest.replace('name = "notes"', 'name = "abd"'))
    check(st == 200 and v["data_kept"] is True, "a complete redeploy after the aborted ones still works", v)
    st, _, _, _ = s.admin("DELETE", "/v1/apps/abd")
    assert st == 200


def atomic_deploy(s, notes_manifest):
    """A deploy is checked in full before anything on disk changes: a component that does not
    link leaves the running version as it was, on disk and after a restart."""
    app = os.path.join(s.data, "apps/notes")
    comps = os.path.join(s.data, "components")

    def snapshot():
        files = {f: open(os.path.join(app, f), "rb").read() for f in ("manifest.toml", "component.sha256", "version")}
        files["cwasm inode"] = os.stat(os.path.join(app, "component.cwasm")).st_ino
        files["store"] = sorted(os.listdir(comps))
        files["app dir"] = sorted(os.listdir(app))
        return files

    def post(name, data, manifest):
        m = base64.b64encode(manifest.encode()).decode()
        st, _, d, _ = s.admin("PUT", f"/v1/apps/{name}", {"x-spared-manifest": m}, data)
        return st, d

    before = snapshot()
    v0 = json.loads(s.admin("GET", "/v1/apps/notes")[2])
    KEEP.update(notes_hash=v0["component_sha256"], notes_version=v0["version"])
    check(len(LINK_FAILS_EMPTY_HANDLER) == 53, "the empty-handler test component is 53 bytes")
    for what, data in [("a 53-byte component that fails to link", LINK_FAILS_EMPTY_HANDLER),
                       ("a component importing an interface the host does not have", LINK_FAILS_UNKNOWN_IMPORT),
                       ("a 53-byte file that is not a component", bytes(53))]:
        st, d = post("notes", data, notes_manifest)
        check(st == 422 and kind(d) == "invalid_component", f"deploying {what} over a working app is refused with invalid_component", (st, d[:300]))
        check(snapshot() == before, f"after {what}, the app's files and the component store are as they were")
        v = json.loads(s.admin("GET", "/v1/apps/notes")[2])
        check((v["version"], v["component_sha256"]) == (v0["version"], v0["component_sha256"]), f"after {what}, the app is listed at its old version and component", v)
        st, _, d, _ = s.app("notes", "/api/notes")
        check(st == 200 and b"kept across redeploy" in d, f"after {what}, the old app serves its data")
    st, d = post("never-was", LINK_FAILS_EMPTY_HANDLER, notes_manifest.replace('name = "notes"', 'name = "never-was"'))
    check(st == 422 and kind(d) == "invalid_component" and not os.path.exists(os.path.join(s.data, "apps/never-was")),
          "a first deploy that does not link leaves no app directory behind", (st, d[:200]))
    st, _, d, _ = s.admin("GET", "/v1/apps/never-was")
    check(st == 404, "and no app is listed under that name")


def deep_trees(s):
    # H3: a data tree nested 15,000 deep (deeper than PATH_MAX) must not crash the server when it
    # is listed, backed up, moved aside by a restore, or deleted.
    st, v = s.deploy("deep", HOSTILE, hostile_manifest("deep", cpu_ms=1000))
    assert st == 200, v
    data = os.path.join(s.data, "apps/deep/data")
    st, b0 = jadmin(s, "POST", "/v1/apps/deep/backup")
    assert st == 200, b0
    t = time.monotonic()
    make_deep(data, 15000)
    built = time.monotonic() - t
    st, _, d, _ = s.admin("GET", "/v1/apps/deep")
    check(st == 200 and json.loads(d)["data_bytes"] >= 0, f"GET /v1/apps/deep answers with data 15,000 levels deep (built in {built:.1f}s)", d[:200])
    st, v = jadmin(s, "POST", "/v1/apps/deep/backup")
    leftovers = [f for f in os.listdir(os.path.join(s.data, "backups/deep")) if f.startswith(".tmp")]
    check(st == 500 and v["error"]["kind"] == "backup_failed" and "nested more than 64" in v["error"]["message"] and "unchanged" in v["error"]["fix"] and not leftovers,
          "a backup of a 15,000-deep tree is refused with a clear limit and leaves nothing behind", v)
    st, r = jadmin(s, "POST", "/v1/apps/deep/restore", {"backup_id": b0["backup_id"]})
    check(st == 200 and os.path.isdir(os.path.join(s.data, "backups/deep", r["pre_restore_id"], "d")), "a restore moves the deep tree aside as a pre-restore copy", r)
    st, v = jadmin(s, "DELETE", f"/v1/apps/deep/backups/{r['pre_restore_id']}")
    check(st == 200 and not os.path.exists(os.path.join(s.data, "backups/deep", r["pre_restore_id"])) and s.p.poll() is None,
          "deleting a pre-restore copy holding the deep tree works and the server stays up", v)
    make_deep(data, 15000)
    t = time.monotonic()
    st, _, d, _ = s.admin("DELETE", "/v1/apps/deep")
    took = time.monotonic() - t
    trash = os.path.join(s.data, "trash")
    check(st == 200 and s.p.poll() is None and not os.path.exists(os.path.join(s.data, "apps/deep")) and os.listdir(trash) == [],
          f"DELETE of an app whose data is 15,000 levels deep removes it whole in {took:.2f}s and the server stays up", (st, d[:200], os.listdir(trash)))
    st, _, _, _ = s.app("notes", "/api/notes")
    check(st == 200, "other apps still answer after the deep delete")
    import shutil as _sh
    _sh.rmtree(os.path.join(s.data, "backups/deep"), ignore_errors=True)


def engine_version(blob):
    """(start, end) of the wasmtime version string in a compiled file: the `.wasmtime.engine` ELF
    section holds a format byte (0), the version's length, the version, then the target triple."""
    import struct
    assert blob[:6] == b"\x7fELF\x02\x01", "the compiled file is not a little-endian ELF64 file"
    shoff, = struct.unpack_from("<Q", blob, 0x28)
    size, count, names = struct.unpack_from("<HHH", blob, 0x3A)
    sec = lambda i: struct.unpack_from("<IIQQQQ", blob, shoff + i * size)
    strtab = sec(names)[4]
    for i in range(count):
        name, _, _, _, off, _ = sec(i)
        if blob[strtab + name:blob.index(b"\0", strtab + name)] == b".wasmtime.engine":
            assert blob[off] == 0, "unknown engine section format"
            return off + 2, off + 2 + blob[off + 1]
    raise AssertionError("no .wasmtime.engine section in the compiled file")


def restart(s, tmp):
    log = open(os.path.join(tmp, "serve.log"), errors="replace").read()
    check("one browser origin" in log, "path routing prints a warning at startup")
    st, _, d, _ = s.admin("GET", "/v1/apps/notes")
    check(st == 200 and json.loads(d)["version"] == 5, "apps are loaded from disk after a restart")
    v = json.loads(d)
    check((v["version"], v["component_sha256"]) == (KEEP["notes_version"], KEEP["notes_hash"]) and "skipping app" not in log,
          "after deploys of components that do not link and a restart, the app is still listed at its old version and component")
    st, _, d, _ = call(s.pub, "GET", "/_/notes/api/notes", f"127.0.0.1:{s.pub}")
    check(st == 200 and b"kept across redeploy" in d, "with --allow-path-routing, /_/notes/ serves the app", d)
    st, h, _, _ = call(s.pub, "GET", "/_/notes", f"127.0.0.1:{s.pub}")
    check(st == 308 and h.get("location") == "/_/notes/", "/_/notes redirects to /_/notes/")
    st, h, _, _ = call(s.pub, "GET", "/_/notes/api/notes?k=PATHFORMSECRET", f"127.0.0.1:{s.pub}")
    time.sleep(0.3)
    log = open(os.path.join(tmp, "serve.log"), errors="replace").read()
    line = {l["request_id"]: l for l in access_lines(log)}.get(h.get("x-spared-request-id"), {})
    check((line.get("app"), line.get("path")) == ("notes", "/_/notes/api/notes") and "PATHFORMSECRET" not in log,
          "a path-form request is logged with its app and no query string", line)
    check(re.search(r"data dir: .* \(format 1\), [1-9]\d* apps loaded", log), "a restart prints how many apps it loaded")
    s.stop()
    # a data dir with no stamp (a new one) must open and be stamped
    stamp_file = os.path.join(s.data, "format.json")
    os.remove(stamp_file)
    log_before = os.path.getsize(os.path.join(tmp, "serve.log"))
    # orphans from a crash are swept at start
    comps = os.path.join(s.data, "components")
    # a compiled file from another wasmtime version: the version tag in its engine section is
    # changed in place (both links see it); the app must load by recompiling the stored wasm
    hfile = os.path.join(s.data, "apps/hostile/component.cwasm")
    hhash = open(os.path.join(s.data, "apps/hostile/component.sha256")).read().strip()
    blob = bytearray(open(hfile, "rb").read())
    vs, ve = engine_version(blob)
    version = bytes(blob[vs:ve])
    assert re.fullmatch(rb"\d+", version), f"unexpected engine version tag {version!r}"
    blob[ve - 1] = ord("1") if version[-1:] == b"0" else ord("0")
    with open(hfile, "r+b") as f:
        f.write(blob)
    check(os.path.isfile(os.path.join(comps, hhash + ".wasm")), "the deployed wasm is kept in the store next to the compiled file")
    # a manifest edited on disk is clamped to the server maxima at load
    path = os.path.join(s.data, "apps/hostile/manifest.toml")
    with open(path, "w") as f:
        f.write(hostile_manifest(cpu_ms=10**12, memory_mb=10**6))
    s2 = Server(tmp, "--max-cpu-ms", "3000", "--access-log", "off")
    try:
        check(json.load(open(stamp_file))["format"] == 1, "a data dir without a format stamp is opened and stamped")
        st, _, d, _ = s2.admin("GET", "/v1/apps/hostile")
        lim = json.loads(d)["limits"]
        check(lim["cpu_ms"] == 3000 and lim["memory_mb"] == 256, "limits edited on disk are clamped at load", lim)
        st, _, d, _ = s2.app("hostile", "/")
        check(st == 200, "the clamped app still serves")
        log2 = open(os.path.join(tmp, "serve.log"), errors="replace").read()
        stored = os.path.join(comps, hhash + ".cwasm")
        fresh = open(stored, "rb").read()
        check("recompiled from the stored wasm" in log2 and os.stat(hfile).st_ino == os.stat(stored).st_ino and fresh[slice(*engine_version(fresh))] == version,
              "a compiled file from another wasmtime version is recompiled from the stored wasm and relinked")
        left = sorted(os.listdir(comps))
        check(not any(f.startswith("0000") or f.endswith(".tmp") for f in left), "orphaned and partial components are swept at start", left)
        st, h, d, _ = s2.app("notes", "/api/notes")
        check(st == 200 and b"kept across redeploy" in d, "a restarted server serves the app with its data")
        time.sleep(0.3)
        with open(os.path.join(tmp, "serve.log"), errors="replace") as f:
            f.seek(log_before)
            new_log = f.read()
        check("access log: off" in new_log and not access_lines(new_log) and re.fullmatch(r"[0-9a-f]{16}", h.get("x-spared-request-id", "")),
              "--access-log off writes no request lines (responses still carry a request id)")
    finally:
        s2.stop()
    format_refusals(s, stamp_file)


def signals(tmp):
    """SIGTERM and SIGINT stop the server gracefully (exit 0, requests in flight finish, the
    listeners close at once), also as PID 1 of a PID namespace, where the kernel drops a signal the
    process has no handler for."""
    manifest = hostile_manifest("hostile", cpu_ms=1000, deadline_ms=20000)
    log_text = lambda s: open(os.path.join(s.tmp, "serve.log"), errors="replace").read()

    def fresh(name, *extra, **kw):
        d = os.path.join(tmp, name)
        os.makedirs(d)
        s = Server(d, "--max-cpu-ms", "8000", "--max-deadline-ms", "20000", *extra, **kw)
        st, v = s.deploy("hostile", HOSTILE, manifest)
        assert st == 200, v
        return s

    def exit_of(s, sig, within=6):
        pid = s.server_pid()
        t = time.monotonic()
        os.kill(pid, sig)
        try:
            rc = s.p.wait(within)
        except subprocess.TimeoutExpired:
            s.p.kill()
            raise SystemExit(f"FAIL - the server did not exit within {within} s of signal {sig}")
        return rc, time.monotonic() - t

    for sig, label in [(signal.SIGTERM, "SIGTERM"), (signal.SIGINT, "SIGINT")]:
        s = fresh(f"sig-{label}")
        rc, t = exit_of(s, sig)
        text = log_text(s)
        check(rc == 0 and t < 2 and "shutting down" in text and "stopped:" in text,
              f"{label} stops an idle server with exit 0 in {t:.2f}s and says so", (rc, text[-300:]))
        refused = False
        try:
            socket.create_connection(("127.0.0.1", s.pub), timeout=1).close()
        except OSError:
            refused = True
        check(refused, f"after {label} the listeners are closed")

    # a request in flight finishes; new connections are refused meanwhile
    s = fresh("sig-inflight")
    got = {}

    def spin_request(s, timeout=30):
        try:
            got["r"] = s.app("hostile", "/spin", timeout=timeout)
        except OSError as e:
            got["r"] = (0, {}, str(e).encode(), 0)

    th = threading.Thread(target=spin_request, args=(s,))
    th.start()
    time.sleep(0.3)
    pid = s.server_pid()
    t0 = time.monotonic()
    os.kill(pid, signal.SIGTERM)
    time.sleep(0.2)
    refused = False
    try:
        socket.create_connection(("127.0.0.1", s.pub), timeout=1).close()
    except OSError:
        refused = True
    th.join()
    rc = s.p.wait(6)
    st, _, d, _ = got["r"]
    check(st == 508 and kind(d) == "cpu_limit" and rc == 0 and refused,
          f"SIGTERM during a request lets it finish (answered {st}, exit {rc} after {time.monotonic() - t0:.1f}s) and refuses new connections", (st, d[:200], rc, refused))

    # a request that outlasts the grace period is cut, and the exit is still clean and prompt
    s = fresh("sig-slow")
    st, v = s.deploy("hostile", HOSTILE, hostile_manifest("hostile", cpu_ms=8000, deadline_ms=20000))
    assert st == 200, v
    th = threading.Thread(target=spin_request, args=(s, 15))
    th.start()
    time.sleep(0.3)
    rc, t = exit_of(s, signal.SIGTERM, within=8)
    th.join()
    check(rc == 0 and 3 < t < 6 and "cut after the grace period" in log_text(s),
          f"a request longer than the grace period is cut and the server exits 0 after {t:.1f}s", (rc, t))

    # a SIGTERM while the examples are still being deployed ends the start-up
    d = os.path.join(tmp, "sig-start")
    os.makedirs(d)
    logf = open(os.path.join(d, "serve.log"), "ab")
    env = dict(os.environ, HOME=os.path.join(d, "home"))
    os.makedirs(env["HOME"])
    p = subprocess.Popen([BIN, "serve", "--data-dir", os.path.join(d, "data"), "--listen", f"127.0.0.1:{free_port()}",
                          "--admin-listen", f"127.0.0.1:{free_port()}", "--examples-dir", DIST], stdout=logf, stderr=logf, env=env)
    for _ in range(100):
        if b"starting" in open(os.path.join(d, "serve.log"), "rb").read():
            break
        time.sleep(0.05)
    t = time.monotonic()
    p.send_signal(signal.SIGTERM)
    try:
        rc = p.wait(20)
    except subprocess.TimeoutExpired:
        p.kill()
        raise SystemExit("FAIL - the server ignored SIGTERM while starting")
    check(rc == 0, f"SIGTERM while the server is still starting exits 0 ({time.monotonic() - t:.1f}s)", rc)

    ep = open(os.path.join(ROOT, "deploy/entrypoint.sh")).read()
    check(re.search(r'^\s*exec setpriv .* "\$@"$', ep, re.M) and re.search(r'^exec "\$@"$', ep, re.M),
          "the container entrypoint execs the server on both paths, so the server is PID 1 and gets the stop signal")

    # PID 1: the kernel delivers no signal to a namespace's init unless it has a handler
    probe = subprocess.run(["unshare", "--pid", "--fork", "--mount-proc", "true"], capture_output=True)
    if probe.returncode != 0:
        print("skip - SIGTERM as PID 1: unshare --pid is not permitted here:", probe.stderr.decode().strip(), flush=True)
        return
    s = fresh("sig-pid1", pidns=True)
    rc, t = exit_of(s, signal.SIGTERM, within=5)
    check(rc == 0 and t < 2, f"as PID 1 of a PID namespace, SIGTERM stops the server with exit 0 in {t:.2f}s", (rc, t))


def descriptors(tmp):
    """Running out of file descriptors must not spin the accept loops, and the server serves
    again once descriptors are free; the soft limit is raised to the hard limit at start."""
    d = os.path.join(tmp, "fds")
    os.makedirs(d)
    s = Server(d, "--client-timeout-secs", "30", nofile=(64, 64))
    pid = s.p.pid
    open_fds = lambda: len(os.listdir(f"/proc/{pid}/fd"))
    st, v = s.deploy("notes", os.path.join(DIST, "notes/app.wasm"), open(os.path.join(DIST, "notes/spared.toml")).read())
    assert st == 200, v
    clients = []
    for _ in range(100):
        try:
            clients.append(socket.create_connection(("127.0.0.1", s.pub), timeout=2))
        except OSError:
            break
    time.sleep(0.5)
    cpu0 = cpu_seconds(pid)
    time.sleep(3)
    used = cpu_seconds(pid) - cpu0
    held = open_fds()
    check(held >= 60, f"the server is out of descriptors ({held} of 64 in use with {len(clients)} clients connected)")
    check(used < 0.3, f"a server out of descriptors does not spin ({used:.2f} CPU-s in 3 s)", used)
    accept_lines = [l for l in open(os.path.join(d, "serve.log"), errors="replace").read().splitlines() if "accept on the" in l]
    check(1 <= len(accept_lines) <= 3, f"accept failures are logged, but once per interval ({len(accept_lines)} lines in 3.5 s)", accept_lines[:5])
    try:
        adm_st, _, _, _ = call(s.adm, "GET", "/v1/apps", f"127.0.0.1:{s.adm}", s.auth, timeout=5)
    except OSError as e:
        adm_st = repr(e)
    check(adm_st == 200, "with every descriptor taken, the admin listener still answers (a descriptor is held back for it)", adm_st)
    for c in clients:
        c.close()
    ok_after = False
    for _ in range(50):
        try:
            st, _, _, _ = call(s.pub, "GET", "/healthz", f"127.0.0.1:{s.pub}", timeout=2)
            st2, _, _, _ = s.admin("GET", "/v1/apps")
            if st == 200 and st2 == 200:
                ok_after = True
                break
        except OSError:
            pass
        time.sleep(0.2)
    check(ok_after, "once descriptors are free again, the public and admin listeners answer")
    st, _, d2, _ = s.app("notes", "/api/notes")
    if st != 200:
        for log in (os.path.join(s.data, "apps/notes/app.log"), os.path.join(d, "serve.log")):
            if os.path.exists(log):
                print(f"--- tail of {log}", flush=True)
                print("".join(open(log, errors="replace").readlines()[-40:]), flush=True)
    check(st == 200, "and an app serves", (st, d2[:200]))
    s.stop()

    # the soft limit is raised to the hard limit
    d = os.path.join(tmp, "fds-raise")
    os.makedirs(d)
    s = Server(d, nofile=(64, 4096))
    limits = open(f"/proc/{s.p.pid}/limits").read()
    m = re.search(r"Max open files\s+(\d+)\s+(\d+)", limits)
    check(m and m.group(1) == m.group(2) == "4096", "at start the soft open-files limit is raised to the hard limit", m and m.groups())
    check("open files: limit 4096 (was 64)" in open(os.path.join(d, "serve.log"), errors="replace").read(), "the startup output says what the limit is")
    s.stop()


def rss_mb(pid):
    for line in open(f"/proc/{pid}/status"):
        if line.startswith("VmRSS"):
            return int(line.split()[1]) // 1024
    return 0


def hostile_reply(s, app, path, **kw):
    st, _, d, t = s.app(app, path, **kw)
    return st, d.decode(errors="replace").strip(), t


def resource_caps(tmp):
    """One app must not be able to take the server down: its directory listings, descriptors,
    log output, data size and request queue are all bounded, and each refusal is an ordinary
    error to the guest (a failed call, not a crash)."""
    d = os.path.join(tmp, "caps")
    os.makedirs(d)
    s = Server(d, "--max-app-entries", "3000", "--max-app-data-mb", "8")
    pid = s.server_pid()
    st, v = s.deploy("atk", HOSTILE, hostile_manifest("atk", cpu_ms=3000, deadline_ms=20000))
    check(st == 200, "deploy the attacker app for the resource caps", v)
    st, v = s.deploy("notes", os.path.join(DIST, "notes/app.wasm"), open(os.path.join(DIST, "notes/spared.toml")).read())
    check(st == 200, "deploy a second app next to it", v)

    # descriptors: an app holds a bounded number open, so it cannot use up the process's
    st, body, _ = hostile_reply(s, "atk", "/fds?n=500")
    m = re.match(r"held (\d+) err=(.*)", body)
    check(st == 200 and m and 30 <= int(m.group(1)) <= 64 and "uota" in m.group(2),
          "an app can hold only --max-app-fds files open; the next open fails with an error", body)
    held = len(os.listdir(f"/proc/{pid}/fd"))
    check(held < 150, f"the server process is not out of descriptors ({held} open)")
    st, _, _, _ = s.admin("GET", "/v1/apps")
    check(st == 200, "and the admin API answers")
    st, _, _, _ = s.admin("POST", "/v1/apps/atk/evict")
    check(st == 200, "evicting the app releases its descriptors", st)

    # directory listings: entries are capped per app, and so is the number of open listings
    st, body, _ = hostile_reply(s, "atk", "/mkfiles?start=0&n=5000")
    m = re.match(r"made (\d+) err=(.*)", body)
    check(st == 200 and m and 2900 <= int(m.group(1)) <= 3000 and "uota" in m.group(2),
          "creating files past --max-app-entries fails with an error, not a crash", body)
    st, body, _ = hostile_reply(s, "atk", "/listcount")
    check(st == 200 and re.fullmatch(r"entries \d+", body) and int(body.split()[1]) <= 3000, "a listing never holds more than the cap", body)
    before = rss_mb(pid)
    st, body, _ = hostile_reply(s, "atk", "/readdirs?n=50")
    check(st == 200 and body.startswith("listed 4 ") and "uota" in body, "an app can hold only a few directory listings open at once", body)
    check(rss_mb(pid) - before < 50, f"holding listings does not grow the server ({before} MB to {rss_mb(pid)} MB)")
    st, _, _ = hostile_reply(s, "notes", "/api/notes")
    check(st == 200, "another app still answers")

    # log bytes: a request's output to the log is cut at the per-request limit
    log = os.path.join(s.data, "apps/atk/app.log")
    before = os.path.getsize(log) if os.path.exists(log) else 0
    st, body, secs = hostile_reply(s, "atk", "/log?n=400")
    check(st == 200 and body.startswith("logged 400 MiB"), "an app that writes 400 MiB to its log is not stopped by it", body)
    grown = os.path.getsize(log) - before
    check(grown <= (1 << 20) + 4096, f"but only the per-request limit reaches the log file ({grown} bytes of 400 MiB)")
    text = open(log, errors="replace").read()
    check(text.count("log output dropped") == 1, "and the log says output was dropped, once")
    for _ in range(20):
        hostile_reply(s, "atk", "/log?n=2")
    check(os.path.getsize(log) < 6 << 20 and not os.path.exists(log + ".2"), "a hot app's log rotates instead of growing", os.path.getsize(log))

    # data bytes: an app's data tree is capped; a full tree is a full disk to the guest
    st, v = s.deploy("atk2", HOSTILE, hostile_manifest("atk2", cpu_ms=3000, deadline_ms=20000))
    check(st == 200, "deploy a second attacker for the data limit", v)
    st, body, _ = hostile_reply(s, "atk2", "/fill?n=20")
    m = re.match(r"wrote (\d+) MiB", body)
    check(st == 200 and m and 5 <= int(m.group(1)) <= 8, "a write stream stops at --max-app-data-mb", body)
    st, body, _ = hostile_reply(s, "atk2", "/fill?n=1")
    check(st == 200 and "No space left" in body, "a full data directory refuses a new write stream with 'no space left on device'", body)
    st, body, _ = hostile_reply(s, "atk2", "/sparse?n=1024")
    check(st == 200 and "No space left" in body, "a sparse file counts at its full size", body)
    tree = sum(os.path.getsize(os.path.join(r, f)) for r, _, fs in os.walk(os.path.join(s.data, "apps/atk2/data")) for f in fs)
    check(tree <= 8 << 20, f"the data directory stays within its limit ({tree} bytes)")
    posted = []
    for _ in range(600):
        st, _, _, _ = s.app("notes", "/api/notes", "POST", json.dumps({"text": "x" * 20000}), {"content-type": "application/json"})
        posted.append(st)
        if st != 201 and posted.count(st) > 2:
            break
    check(201 in posted and posted[-1] != 201, "a SQLite app gets 'database or disk is full' once its data is at the limit", posted[-3:])
    nlog = open(os.path.join(s.data, "apps/notes/app.log"), errors="replace").read()
    check("database or disk is full" in nlog, "that is the error SQLite reports", nlog[:300])
    st, _, _ = hostile_reply(s, "notes", "/api/notes")
    check(st == 200, "and the data written before is still readable")
    s.stop()

    # sparse files: a backup keeps the holes, and one whose files add up to more than the data
    # limit is refused whole
    d = os.path.join(tmp, "caps-sparse")
    os.makedirs(d)
    s = Server(d)
    st, v = s.deploy("atk", HOSTILE, hostile_manifest("atk", cpu_ms=3000, deadline_ms=20000))
    check(st == 200, "deploy the attacker app for the sparse backup", v)
    st, body, _ = hostile_reply(s, "atk", "/sparse?n=900")
    check(st == 200 and body == "sparse ok", "an app makes a 900 MB sparse file (within the data limit)", body)
    t0 = time.monotonic()
    st, _, data, _ = s.admin("POST", "/v1/apps/atk/backup")
    took = time.monotonic() - t0
    v = json.loads(data)
    check(st == 200 and v["bytes"] == 900 << 20, "the backup of a sparse file succeeds and reports its apparent size", v)
    disk = sum(os.stat(os.path.join(r, f)).st_blocks * 512 for r, _, fs in os.walk(os.path.join(s.data, "backups/atk")) for f in fs)
    check(disk < 50 << 20, f"the copy keeps the holes ({disk >> 20} MB on disk for 900 MB of file, {took:.1f} s)")
    big = os.path.join(s.data, "apps/atk/data/huge.bin")
    with open(big, "wb") as f:
        f.truncate(3 << 30)
    st, _, data, _ = s.admin("POST", "/v1/apps/atk/backup")
    check(st == 500 and kind(data) == "backup_failed" and b"nothing was copied" in data, "files that add up to more than the data limit are refused", data[:300])
    ids = os.listdir(os.path.join(s.data, "backups/atk"))
    check(len([i for i in ids if not i.startswith(".")]) == 1 and not any(i.startswith(".tmp") for i in ids), "and the refused backup leaves nothing behind", ids)
    s.stop()

    # queue: a slow app holds a bounded number of requests, so the other apps keep their share
    d = os.path.join(tmp, "caps-queue")
    os.makedirs(d)
    s = Server(d, "--max-connections", "8", "--max-app-queue", "4")
    st, v = s.deploy("slow", HOSTILE, hostile_manifest("slow", deadline_ms=20000))
    check(st == 200, "deploy a slow app", v)
    st, v = s.deploy("notes", os.path.join(DIST, "notes/app.wasm"), open(os.path.join(DIST, "notes/spared.toml")).read())
    check(st == 200, "deploy a quick app beside it", v)
    replies = []

    def slow_client():
        st, h, data, secs = s.app("slow", "/sleep?ms=600")
        replies.append((st, h.get("retry-after"), kind(data), secs))
    clients = [threading.Thread(target=slow_client) for _ in range(8)]
    for c in clients:
        c.start()
    time.sleep(0.3)
    st, _, _, secs = s.app("notes", "/api/notes", timeout=10)
    check(st == 200 and secs < 0.3, f"with eight clients on a slow app and eight connections allowed, another app answers at once ({secs:.2f} s)")
    for c in clients:
        c.join()
    busy = [r for r in replies if r[0] == 503]
    check(len(busy) == 4 and all(r[1] == "1" and r[2] == "app_busy" for r in busy), "past --max-app-queue a slow app's requests get 503 app_busy with Retry-After", replies)
    check(sum(1 for r in replies if r[0] == 200) == 4, "and the requests within the limit are answered", replies)
    st, h, data, _ = s.app("slow", "/")
    check(st == 200, "the app takes requests again once the queue has drained")
    s.stop()


def tree_usage(s, app):
    """Apparent bytes and entries (files, directories, links) of an app's data directory."""
    nbytes = entries = 0
    for root, dirs, files in os.walk(os.path.join(s.data, "apps", app, "data")):
        entries += len(dirs) + len(files)
        nbytes += sum(os.lstat(os.path.join(root, f)).st_size for f in files)
    return nbytes, entries


def set_limits(s, app, **limits):
    st, _, d, _ = s.admin("PUT", f"/v1/apps/{app}/limits", body=json.dumps(limits).encode())
    check(st == 200, f"set limits {limits} for {app}", d)
    return json.loads(d)


def fs_books(tmp, per_app=False):
    """The per-app data, entry and descriptor caps hold against a guest that tells the host
    things that are not true: a file renamed onto itself, a hard link that is then unlinked, a
    stream opened before its file was truncated, and streams kept after the descriptor is gone.
    With per_app, the 8 MB and 100 entries come from each app's own limits under larger server
    caps, so the same books run with the per-app numbers."""
    d = os.path.join(tmp, "books-per-app" if per_app else "books")
    os.makedirs(d)
    if per_app:
        s = Server(d, "--max-app-data-mb", "512", "--max-app-entries", "5000", "--max-app-fds", "8")
    else:
        s = Server(d, "--max-app-data-mb", "8", "--max-app-entries", "100", "--max-app-fds", "8")
    pid = s.server_pid()
    for app in ["rename", "entries", "links", "stale", "streams", "readers", "cycle"]:
        st, v = s.deploy(app, HOSTILE, hostile_manifest(app, cpu_ms=5000, deadline_ms=30000))
        check(st == 200, f"deploy the {app} attacker for the books", v)
        if per_app:
            set_limits(s, app, data_mb=8, entries=100)

    st, body, _ = hostile_reply(s, "rename", "/selfrename?n=40&mb=1&trick=1", timeout=60)
    nbytes, _ = tree_usage(s, "rename")
    check(st == 200 and nbytes <= 8 << 20, f"renaming each file onto itself does not credit its size back ({nbytes >> 20} MB of an 8 MB limit)", body)
    st, body, _ = hostile_reply(s, "rename", "/selfrename-entries?n=400&trick=1", timeout=60)
    _, entries = tree_usage(s, "rename")
    check(st == 200 and entries <= 100, f"nor does it credit an entry back ({entries} entries of a limit of 100)", body)

    st, body, _ = hostile_reply(s, "links", "/selfrename?n=1&mb=1")
    check(st == 200 and body.startswith("made 1 "), "the link attacker writes its first file", body)
    for _ in range(4):
        hostile_reply(s, "links", "/fill?n=20", timeout=60)
        st, body, _ = hostile_reply(s, "links", "/linkcycle?n=30", timeout=60)
        check(st == 200 and body.startswith("link cycles "), "it links a file and unlinks the link, over and over", body)
    nbytes, _ = tree_usage(s, "links")
    check(nbytes <= 8 << 20, f"unlinking a hard link does not credit the file's size back ({nbytes >> 20} MB of an 8 MB limit)")

    st, body, _ = hostile_reply(s, "stale", "/stale?n=40&mb=1", timeout=60)
    nbytes, _ = tree_usage(s, "stale")
    check(st == 200 and nbytes <= 8 << 20, f"a stream opened before the file was truncated is charged for what it writes ({nbytes >> 20} MB of an 8 MB limit)", body)

    base = len(os.listdir(f"/proc/{pid}/fd"))
    st, body, _ = hostile_reply(s, "streams", "/streams?n=90")
    m = re.match(r"streams held (\d+) err=(.*)", body)
    check(st == 200 and m and int(m.group(1)) <= 8 and "uota" in m.group(2), "write streams count against --max-app-fds even when the descriptor is gone", body)
    held = len(os.listdir(f"/proc/{pid}/fd")) - base
    check(held <= 12, f"the server holds about --max-app-fds more descriptors, not 90 ({held} more)")
    st, body, _ = hostile_reply(s, "readers", "/streams?n=90&read=1")
    m = re.match(r"streams held (\d+) err=(.*)", body)
    check(st == 200 and m and int(m.group(1)) <= 8 and "uota" in m.group(2), "so do read streams", body)
    st, body, _ = hostile_reply(s, "cycle", "/streamcycle?n=60")
    check(st == 200 and body.startswith("stream cycles 60 err="), "a stream that is dropped gives its descriptor slot back", body)
    s.stop()


def per_app_limits(tmp):
    """An app at the data limit its operator set gets the usual full-disk error while another app
    under the same server keeps writing up to the server's cap; raising the limit gives room
    back, and clearing it restores the server's cap."""
    d = os.path.join(tmp, "per-app")
    os.makedirs(d)
    s = Server(d, "--max-app-data-mb", "24")
    for app in ["small", "big"]:
        st, v = s.deploy(app, HOSTILE, hostile_manifest(app, cpu_ms=5000, deadline_ms=30000))
        check(st == 200, f"deploy {app} for per-app limits", v)
    v = set_limits(s, "small", data_mb=4)
    check(v["effective"]["data_mb"] == 4 and v["server_caps"]["data_mb"] == 24, "small's limit is 4 MB under a 24 MB cap", v)
    st, body, _ = hostile_reply(s, "small", "/fill?n=10", timeout=60)
    nbytes, _ = tree_usage(s, "small")
    check(st == 200 and body.startswith("wrote ") and "err=" in body and not body.endswith("err=") and nbytes <= 4 << 20,
          f"small stops at its own 4 MB with an error from the write ({nbytes >> 20} MB)", body)
    st, body, _ = hostile_reply(s, "big", "/fill?n=10", timeout=60)
    check(st == 200 and body == "wrote 10 MiB err=", "big, with no limits of its own, keeps writing past small's 4 MB", body)
    st, body, _ = hostile_reply(s, "big", "/fill?n=20", timeout=60)
    nbytes, _ = tree_usage(s, "big")
    check(st == 200 and nbytes <= 24 << 20 and not body.endswith("err="), f"and stops at the server's 24 MB cap ({nbytes >> 20} MB)", body)
    st, _, d_, _ = s.admin("GET", "/v1/apps")
    apps = {a["name"]: a for a in json.loads(d_)["apps"]}
    check(apps["small"]["quota"]["effective"]["data_mb"] == 4 and apps["big"]["quota"]["effective"]["data_mb"] == 24
          and apps["small"]["data_bytes"] <= 4 << 20, "spared ps shows each app's limit and use", apps["small"].get("quota"))
    v = set_limits(s, "small", data_mb=8)
    check(v["evicted"] is True or v["evicted"] is False, "raising small's limit answers", v)
    st, body, _ = hostile_reply(s, "small", "/fill?n=2", timeout=60)
    check(st == 200 and body == "wrote 2 MiB err=", "a higher limit gives small room at its next wake", body)
    st, _, d_, _ = s.admin("DELETE", "/v1/apps/small/limits")
    check(st == 200 and json.loads(d_)["effective"]["data_mb"] == 24, "clearing small's limits restores the server's cap", d_)
    st, body, _ = hostile_reply(s, "small", "/fill?n=4", timeout=60)
    check(st == 200 and body == "wrote 4 MiB err=", "and small writes past 8 MB again", body)
    s.stop()


def egress_budget(tmp):
    """An app's outbound HTTP calls run until its egress budget is spent and then fail with
    egress_budget_exhausted, while a neighbor fetching the same host is untouched. Needs
    http://example.com/ to be reachable from this machine without a proxy, as the escape suite's
    controls do; skipped otherwise."""
    try:
        with socket.create_connection(("example.com", 80), timeout=5) as c:
            c.sendall(b"GET / HTTP/1.1\r\nHost: example.com\r\nConnection: close\r\n\r\n")
            c.settimeout(5)
            if not c.recv(16).startswith(b"HTTP/1."):
                raise OSError("no HTTP answer")
    except OSError as e:
        print(f"skip - example.com is not reachable from here ({e}); the egress budget is tested in src/sandbox.rs", flush=True)
        return
    d = os.path.join(tmp, "egress")
    os.makedirs(d)
    s = Server(d)
    for app in ["net", "peer"]:
        st, v = s.deploy(app, HOSTILE, hostile_manifest(app, cpu_ms=5000, deadline_ms=30000))
        check(st == 200, f"deploy {app} for the egress budget", v)
    st, body, _ = hostile_reply(s, "net", "/fetch?n=1", timeout=60)
    m = re.match(r"fetched 1 bytes=(\d+) err=$", body)
    check(st == 200 and m, "an app with no budget fetches example.com", body)
    one = int(m.group(1)) if m else 1
    set_limits(s, "net", egress_bytes_per_hour=one * 3 + one // 2)
    st, body, _ = hostile_reply(s, "net", "/fetch?n=10", timeout=60)
    m = re.match(r"fetched (\d+) bytes=\d+ err=(.*)", body)
    check(st == 200 and m and 1 <= int(m.group(1)) <= 4 and "egress_budget_exhausted" in m.group(2),
          "calls succeed up to the budget, then fail with egress_budget_exhausted", body)
    st, body, _ = hostile_reply(s, "net", "/fetch?n=1", timeout=60)
    check(st == 200 and "fetched 0 " in body and "egress_budget_exhausted" in body, "and keep failing once it is spent", body)
    st, body, _ = hostile_reply(s, "peer", "/fetch?n=5", timeout=60)
    check(st == 200 and body.startswith("fetched 5 ") and body.endswith("err="), "a neighbor keeps fetching", body)
    st, _, d_, _ = s.admin("GET", "/v1/apps/net/limits")
    u = json.loads(d_)["usage"]
    check(u["egress_refused"] >= 2 and u["egress_bytes_left"] == 0, "spared limits show counts the refusals", u)
    s.stop()


def body_budget(tmp):
    """Request bodies are read before a request takes its place in the app's queue, so the server
    holds all of them in memory at once. A total (--max-body-buffer-mb) is reserved before a body
    is read, from its content length when the client states one: past it a request gets 503 with
    Retry-After, and a body the size of --max-request-mb can still proceed alone."""
    import select

    d = os.path.join(tmp, "budget")
    os.makedirs(d)
    s = Server(d, "--max-request-mb", "8", "--max-body-buffer-mb", "16", "--client-timeout-secs", "30")
    st, v = s.deploy("atk", HOSTILE, hostile_manifest("atk", cpu_ms=3000, deadline_ms=20000))
    check(st == 200, "deploy the app that takes uploads", v)
    pid = s.server_pid()
    before = rss_mb(pid)
    full = 8 << 20
    held, refused = [], []
    for _ in range(8):
        c = socket.create_connection(("127.0.0.1", s.pub))
        c.sendall(f"POST / HTTP/1.1\r\nHost: atk.localhost:{s.pub}\r\nContent-Length: {full}\r\n\r\n".encode())
        if select.select([c], [], [], 0.7)[0]:
            refused.append((c, c.recv(65536)))
            continue
        c.sendall(b"x" * (full - 4096))
        held.append(c)
    time.sleep(0.5)
    grown = rss_mb(pid) - before
    check(len(held) == 2 and len(refused) == 6, f"with 16 MB of budget and 8 MB bodies, two uploads are taken and six refused ({len(held)} and {len(refused)})")
    reply = refused[0][1].decode(errors="replace")
    check(reply.startswith("HTTP/1.1 503") and "retry-after: 1" in reply.lower() and '"kind":"server_busy"' in reply,
          "a refused upload gets 503 server_busy with Retry-After", reply[:300])
    check(grown < 40, f"eight stalled 8 MB uploads grow the server by about the budget, not by 64 MB ({grown} MB)")
    st, _, _, _ = s.app("atk", "/", "POST", b"x" * 1000)
    check(st == 503, "a small request is refused too while the budget is used up", st)
    st, _, _, _ = s.admin("GET", "/v1/apps")
    check(st == 200, "and the admin API still answers")
    for c, _ in refused:
        c.close()
    for c in held:
        c.close()
    time.sleep(0.5)
    st, _, _, _ = s.app("atk", "/", "POST", b"x" * (4 << 20))
    check(st == 200, "once the stalled clients are gone the budget is free again", st)
    st, _, data, _ = s.app("atk", "/", "POST", b"x" * (9 << 20))
    check(st == 413 and kind(data) == "body_too_large", "a body over --max-request-mb is still refused as too large", (st, data[:200]))
    s.stop()

    d = os.path.join(tmp, "budget-small")
    os.makedirs(d)
    s = Server(d, "--max-request-mb", "8", "--max-body-buffer-mb", "1")
    st, v = s.deploy("atk", HOSTILE, hostile_manifest("atk", cpu_ms=3000, deadline_ms=20000))
    check(st == 200, "deploy the app for a budget smaller than one request", v)
    st, _, _, _ = s.app("atk", "/", "POST", b"x" * (7 << 20))
    check(st == 200, "a budget below --max-request-mb is raised to it, so one 7 MB request proceeds alone", st)
    s.stop()


def store_without_links(tmp):
    """On a file system that cannot hard link, each app directory holds its own copy of its
    component and the store file's link count stays 1. Restarts must not read that as unused:
    the app has to survive any number of them. The store goes on /dev/shm, another file system."""
    d = os.path.join(tmp, "nolinks")
    data = os.path.join(d, "data")
    os.makedirs(data)
    if not (os.path.isdir("/dev/shm") and os.stat("/dev/shm").st_dev != os.stat(d).st_dev):
        print("skip - no second file system for a component store without hard links", flush=True)
        return
    store = tempfile.mkdtemp(dir="/dev/shm", prefix="spared-components.")
    os.symlink(store, os.path.join(data, "components"))
    try:
        s = Server(d)
        st, v = s.deploy("a", HOSTILE, hostile_manifest("a", cpu_ms=3000, deadline_ms=20000))
        check(st == 200, "deploy an app onto a component store on another file system", v)
        s.stop()
        stored = [f for f in os.listdir(store) if f.endswith(".cwasm")]
        check(len(stored) == 1 and os.stat(os.path.join(store, stored[0])).st_nlink == 1, "the store file has one link: the app directory holds a copy", stored)
        for n in (1, 2, 3):
            s = Server(d)
            st, _, body, _ = s.app("a", "/")
            check(st == 200 and body == b"ok\n", f"the app still answers after restart {n}", (st, body[:200]))
            s.stop()
        for f in os.listdir(store):
            os.unlink(os.path.join(store, f))
        s = Server(d)
        st, _, body, _ = s.app("a", "/")
        check(st == 200 and body == b"ok\n", "an app whose store file is gone gets it back from its own copy", (st, body[:200]))
        st, _, _, _ = s.admin("DELETE", "/v1/apps/a")
        check(st == 200 and not any(f.endswith(".cwasm") for f in os.listdir(store)), "deleting it leaves nothing in the store")
        s.stop()
    finally:
        shutil.rmtree(store, ignore_errors=True)


def startup_refusals(tmp):
    """A server that cannot have its ports or its data dir says so in the usual error format and
    leaves the data dir as it found it."""
    env = dict(os.environ, HOME=os.path.join(tmp, "home-refusals"))
    env.pop("SPARED_ADMIN_TOKEN", None)
    os.makedirs(env["HOME"], exist_ok=True)

    def serve(data, pub, adm, *flags):
        return subprocess.run([BIN, *flags, "serve", "--data-dir", data, "--listen", f"127.0.0.1:{pub}", "--admin-listen", f"127.0.0.1:{adm}"],
                              env=env, capture_output=True, text=True, timeout=30)

    taken = socket.socket()
    taken.bind(("127.0.0.1", 0))
    taken.listen(1)
    port = taken.getsockname()[1]
    fresh = os.path.join(tmp, "refused-fresh")
    for label, pub, adm, flag in [("public", port, free_port(), "--listen"), ("admin", free_port(), port, "--admin-listen")]:
        r = serve(fresh, pub, adm)
        check(r.returncode == 1 and f"error[address_in_use]: 127.0.0.1:{port} is already in use" in r.stderr and "fix: stop the other" in r.stderr and flag in r.stderr,
              f"a {label} port that is taken is refused with an error and a fix", r.stderr)
        check("os error" not in r.stderr and "Error:" not in r.stderr, "without the raw operating system error")
        check(not os.path.exists(fresh), "and nothing is created in the data dir")
        r = serve(fresh, pub, adm, "--json")
        e = json.loads(r.stdout)["error"]
        check(r.returncode == 1 and e["kind"] == "address_in_use" and str(port) in e["message"] and e["fix"], "--json reports it as an error object", r.stdout)
    taken.close()

    bad = os.path.join(tmp, "refused-flags")
    for label, args, extra_env, kind, text in [
        ("a public admin address", ["--admin-listen", "0.0.0.0:%d" % free_port()], {}, "admin_bind_refused", "--admin-public-i-know"),
        ("a $PORT that is not a number", [], {"PORT": "abc"}, "invalid_port_env", "$PORT is 'abc'"),
    ]:
        base = [BIN, "serve", "--data-dir", bad, *args]
        if not args:
            base += ["--admin-listen", f"127.0.0.1:{free_port()}"]
        r = subprocess.run(base, env=dict(env, **extra_env), capture_output=True, text=True, timeout=30)
        check(r.returncode == 1 and f"error[{kind}]: " in r.stderr and text in r.stderr + r.stdout and "fix: " in r.stderr, f"{label} is refused as {kind} with a fix", r.stderr)
        r = subprocess.run([BIN, "--json", *base[1:]], env=dict(env, **extra_env), capture_output=True, text=True, timeout=30)
        e = json.loads(r.stdout)["error"]
        check(r.returncode == 1 and e["kind"] == kind and e["fix"], f"--json reports {kind} as an error object", r.stdout)
    check(not os.path.exists(bad), "and nothing is created in the data dir")

    d = os.path.join(tmp, "locked")
    os.makedirs(d)
    s = Server(d)
    lock = os.path.join(s.data, ".lock")
    check(open(lock).read().strip() == str(s.p.pid), "a running server holds <data-dir>/.lock and writes its pid in it")
    token_before = open(os.path.join(s.data, "admin-token")).read()
    r = serve(s.data, free_port(), free_port())
    check(r.returncode == 1 and "error[data_dir_in_use]: " in r.stderr and f"process {s.p.pid}" in r.stderr and "fix: " in r.stderr,
          "a second server on the same data dir is refused with data_dir_in_use", r.stderr)
    r = serve(s.data, free_port(), free_port(), "--json")
    e = json.loads(r.stdout)["error"]
    check(r.returncode == 1 and e["kind"] == "data_dir_in_use" and e["fix"], "--json reports it as an error object", r.stdout)
    check(open(lock).read().strip() == str(s.p.pid) and open(os.path.join(s.data, "admin-token")).read() == token_before,
          "the refused server leaves the lock file and the token as they were")
    st, _, _, _ = s.admin("GET", "/v1/apps")
    check(st == 200, "and the first server keeps serving")
    s.stop()
    s = Server(d)
    check(open(lock).read().strip() == str(s.p.pid), "after the first server exits, the data dir can be started again")
    s.p.kill()
    s.p.wait()
    s = Server(d)
    st, _, _, _ = s.admin("GET", "/v1/apps")
    check(st == 200, "and after a kill -9, which leaves the lock file behind")
    s.stop()

    proj = os.path.join(tmp, "dev-project")
    os.makedirs(proj)
    with open(os.path.join(proj, "spared.toml"), "w") as f:
        f.write('name = "devapp"\n[grants]\ndata = true\n')
    held = []
    for p in (8080, 8081):
        try:
            sk = socket.socket()
            sk.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            sk.bind(("127.0.0.1", p))
            sk.listen(1)
            held.append(sk)
        except OSError:
            pass
    t0 = time.monotonic()
    r = subprocess.run([BIN, "--json", "dev"], cwd=proj, env=env, capture_output=True, text=True, timeout=30)
    e = json.loads(r.stdout)["error"]
    check(r.returncode == 1 and e["kind"] == "address_in_use" and "127.0.0.1:808" in e["message"] and "token" not in json.dumps(e).lower(),
          "spared dev with its ports taken fails with the same error, not with a complaint about the token", r.stdout)
    check(time.monotonic() - t0 < 5 and not os.path.exists(os.path.join(proj, ".spared/admin-token")), "at once, and before it writes a token")
    for sk in held:
        sk.close()


def compile_children(pid):
    out = subprocess.run(["pgrep", "-P", str(pid), "-f", "compile-worker"], capture_output=True, text=True).stdout.split()
    return out


def compile_bombs(tmp):
    """A small component that is expensive to compile must not take the server down. The compiler
    runs in a child process with a memory limit and a time limit, and a component whose functions
    are far larger than any real app's is refused before that."""
    d = os.path.join(tmp, "bombs")
    os.makedirs(d)
    notes = os.path.join(DIST, "notes/app.wasm")
    notes_manifest = open(os.path.join(DIST, "notes/spared.toml")).read()

    def put(s, name, wasm):
        manifest = hostile_manifest(name)
        t = time.monotonic()
        st, _, data, _ = s.admin("PUT", f"/v1/apps/{name}", {"x-spared-manifest": base64.b64encode(manifest.encode()).decode()}, wasm)
        return st, data, time.monotonic() - t

    s = Server(d)
    pid = s.server_pid()
    st, v = s.deploy("notes", notes, notes_manifest)
    check(st == 200, "the defaults deploy a real app", v)
    st, data, secs = put(s, "bomb", compile_bomb(8000, 8000))
    check(st == 422 and kind(data) == "component_too_complex" and secs < 5, f"a 96 KB component with 8,000 locals in one function is refused at once with component_too_complex ({secs:.1f} s)", (st, data[:300]))
    check("locals" in json.loads(data)["error"]["message"], "and the message names what was too large", data[:300])
    check(s.app("notes", "/api/notes")[0] == 200 and s.admin("GET", "/v1/apps")[0] == 200, "the server and the other app are fine afterwards")
    check(s.admin("GET", "/v1/apps/bomb")[0] == 404, "nothing was deployed under the bomb's name")
    s.stop()

    s = Server(d, "--max-function-locals", "100000", "--max-function-kb", "100000", "--compile-memory-mb", "384", "--compile-timeout-secs", "60")
    pid = s.server_pid()
    st, v = s.deploy("notes", notes, notes_manifest)
    check(st == 200, "a real app compiles within a 384 MB compile limit", v)
    peak = [0]
    stop = threading.Event()

    def watch():
        while not stop.is_set():
            peak[0] = max(peak[0], rss_mb(pid))
            time.sleep(0.05)
    w = threading.Thread(target=watch)
    w.start()
    st, data, secs = put(s, "bomb", compile_bomb(6000, 6000))
    stop.set()
    w.join()
    check(st == 422 and kind(data) == "component_too_complex", f"a bomb that passes the size checks hits the compile memory limit and is refused with component_too_complex ({secs:.1f} s)", (st, data[:300]))
    check("memory" in json.loads(data)["error"]["message"], "and the message says it ran out of memory", data[:300])
    check(peak[0] < 300, f"the server's own memory stayed small while it compiled ({peak[0]} MB)")
    check(not compile_children(pid), "no compile process is left running")
    check(s.app("notes", "/api/notes")[0] == 200 and s.admin("GET", "/v1/apps")[0] == 200, "the server and the other app are fine afterwards")
    s.stop()

    s = Server(d, "--max-function-locals", "100000", "--compile-memory-mb", "6000", "--compile-timeout-secs", "2")
    st, data, secs = put(s, "slowbomb", compile_bomb(4000, 4000))
    check(st == 422 and kind(data) == "component_too_complex" and 1.5 < secs < 8, f"a bomb that compiles too slowly is stopped at --compile-timeout-secs ({secs:.1f} s)", (st, data[:300]))
    check("longer than 2 s" in json.loads(data)["error"]["message"], "and the message says it took too long", data[:300])
    check(not compile_children(s.server_pid()), "the compile process is gone")
    s.stop()

    s = Server(d, "--max-component-mb", "1")
    st, v = s.deploy("big", notes, notes_manifest.replace("notes", "big"))
    check(st == 413 and v["error"]["kind"] == "component_too_large", "--max-component-mb refuses a larger component", v)
    s.stop()

    os.makedirs(os.path.join(d, "fresh"))
    s = Server(os.path.join(d, "fresh"), "--max-function-kb", "16")
    st, v = s.deploy("notes", notes, notes_manifest)
    check(st == 422 and v["error"]["kind"] == "component_too_complex" and "function" in v["error"]["message"], "--max-function-kb refuses a component with a function over the limit", v)
    s.stop()

    s = Server(d)
    garbage = b"\0asm\x0d\0\x01\0\xff\xff\xff"
    st, data, _ = put(s, "junk", garbage)
    check(st == 422 and kind(data) == "invalid_component", "a malformed component still gets invalid_component", (st, data[:300]))
    s.stop()


def backup_races(tmp):
    """A process that keeps swapping directories in an app's data tree for a link to a directory
    outside it must not get that directory's files into a backup (the swap is what a guest could
    do between the host looking at an entry and opening it)."""
    import ctypes
    libc = ctypes.CDLL(None, use_errno=True)
    d = os.path.join(tmp, "races")
    os.makedirs(d)
    s = Server(d)
    st, v = s.deploy("notes", os.path.join(DIST, "notes/app.wasm"), open(os.path.join(DIST, "notes/spared.toml")).read())
    check(st == 200, "deploy an app to race against", v)
    data = os.path.join(s.data, "apps/notes/data")
    outside = os.path.join(d, "outside")
    os.makedirs(outside)
    with open(os.path.join(outside, "secret.txt"), "w") as f:
        f.write("HOST SECRET " * 100)
    count = 400
    for i in range(count):
        os.makedirs(os.path.join(data, f"d{i}"))
        with open(os.path.join(data, f"d{i}", "f"), "w") as f:
            f.write("x")
        os.symlink(outside, os.path.join(data, f"l{i}"))
    stop = threading.Event()

    def swap():
        while not stop.is_set():
            for i in range(count):
                libc.renameat2(-100, os.path.join(data, f"d{i}").encode(), -100, os.path.join(data, f"l{i}").encode(), 2)
    t = threading.Thread(target=swap)
    t.start()
    leaked, done = 0, 0
    try:
        for _ in range(6):
            st, _, body, _ = s.admin("POST", "/v1/apps/notes/backup")
            if st != 200:
                continue
            done += 1
            root = os.path.join(s.data, "backups/notes", json.loads(body)["backup_id"])
            if any("secret.txt" in files for _, _, files in os.walk(root)):
                leaked += 1
    finally:
        stop.set()
        t.join()
    check(done >= 3, f"backups of a tree that is being swapped succeed ({done} of 6)")
    check(leaked == 0, f"no backup holds a file from outside the data directory ({leaked} of {done} did)")
    st, _, _, _ = s.admin("GET", "/v1/apps/notes")
    check(st == 200, "and the app's stats are still served while its tree is swapped")
    s.stop()


def host_work(tmp):
    """Work the server does for an app's calls (here, listing a 40,000-entry directory over and
    over) counts against cpu_ms as well as the app's own code, so a loop of such calls is stopped
    at the cpu limit, not left to run until deadline_ms."""
    d = os.path.join(tmp, "hostwork")
    os.makedirs(d)
    s = Server(d)
    st, v = s.deploy("hw", HOSTILE, hostile_manifest("hw", cpu_ms=5000, deadline_ms=30000))
    check(st == 200, "deploy the host-work app", v)
    for start in range(0, 40000, 8000):
        st, body, _ = hostile_reply(s, "hw", f"/mkfiles?start={start}&n=8000", timeout=60)
        check(st == 200 and body.startswith("made 8000 "), f"create 8,000 files ({start} so far)", (st, body))
    st, v = s.deploy("hw", HOSTILE, hostile_manifest("hw", cpu_ms=300, deadline_ms=30000))
    check(st == 200 and v["data_kept"], "redeploy with cpu_ms = 300, keeping the files", v)
    st, body, secs = hostile_reply(s, "hw", "/hostwork?ms=6000", timeout=60)
    check(st == 508 and "cpu_limit" in body, f"a loop of directory listings is stopped by cpu_ms, not left to run ({st} after {secs:.1f} s)", (st, body))
    check(secs < 1.2, f"and it is stopped within about the cpu_ms allowance ({secs:.2f} s for cpu_ms = 300)")
    st, v = s.deploy("hw", HOSTILE, hostile_manifest("hw", cpu_ms=5000, deadline_ms=1500))
    check(st == 200, "redeploy with cpu_ms = 5000 and deadline_ms = 1500", v)
    st, body, secs = hostile_reply(s, "hw", "/hostwork?ms=6000", timeout=60)
    check(st == 504 and 1.2 < secs < 4.0, f"with a cpu_ms larger than the deadline, deadline_ms still ends the request ({st} after {secs:.1f} s)", (st, body))
    s.stop()


def format_refusals(s, stamp_file):
    """A data dir stamped with a format this build does not read is refused and left untouched."""
    good = open(stamp_file).read()
    env = dict(os.environ, HOME=s.home)
    env.pop("SPARED_ADMIN_TOKEN", None)
    args = ["serve", "--data-dir", s.data, "--listen", f"127.0.0.1:{s.pub}", "--admin-listen", f"127.0.0.1:{s.adm}"]

    def tree():
        out = []
        for root, dirs, files in os.walk(s.data):
            dirs[:] = sorted(d for d in dirs if d != "trash")
            out += [(os.path.join(root, f), os.stat(os.path.join(root, f)).st_mtime_ns) for f in sorted(files)]
        return out

    for stamp, want, hint in [
        ('{"format":3,"created_by":"0.0.1-preview","last_opened_by":"0.9.0"}', "data_dir_too_new", "spared 0.9.0 or newer"),
        ('{"format":0,"created_by":"0.0.0","last_opened_by":"0.0.0"}', "data_dir_too_old", "docs/upgrading.md"),
        ("not a stamp", "data_dir_stamp_unreadable", "delete the file"),
    ]:
        with open(stamp_file, "w") as f:
            f.write(stamp)
        before = tree()
        r = subprocess.run([BIN, *args], env=env, capture_output=True, text=True, timeout=30)
        check(r.returncode == 1 and f"error[{want}]: " in r.stderr and "fix: " in r.stderr and hint in r.stderr,
              f"a data dir with stamp {stamp[:14]!r} is refused at start with {want} and a fix", r.stderr)
        r = subprocess.run([BIN, "--json", *args], env=env, capture_output=True, text=True, timeout=30)
        e = json.loads(r.stdout)["error"]
        check(r.returncode == 1 and e["kind"] == want and hint in e["fix"], f"--json reports {want} as an error object", r.stdout)
        check(tree() == before, f"a refused data dir ({want}) is left exactly as it was")
    with open(stamp_file, "w") as f:
        f.write(good)


if __name__ == "__main__":
    main()
