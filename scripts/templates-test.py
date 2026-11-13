#!/usr/bin/env python3
"""Build, deploy and exercise every `spared new --template` template.

For each template listed by `spared templates --json` this scaffolds an app into a temp dir, runs
`spared test`, deploys it to a scratch server and runs a small scripted behavioural check
(create, read, redirect, escape, auth failure and success). It prints PASS or FAIL per template
and the total runtime, and exits nonzero if anything failed.

Run from anywhere:  python3 -I scripts/templates-test.py [template ...]
Needs: target/release/spared (or $SPARED_BIN), the wasm32-wasip2 target and wasi-sdk 25
($WASI_SDK_PATH or ~/.spared/wasi-sdk). Uses ports in 19300-19399, a temp data dir and a temp HOME;
never touches 8080/8081 or ~/.spared/token. CARGO_TARGET_DIR (default: a temp dir, so the first
build compiles SQLite, about a minute; set it to a persistent dir to make reruns fast).
The fetcher check needs outbound HTTPS; without it the check passes only on the app's own error
JSON. Set TEMPLATES_TEST_STRICT_NET=1 to require a real 200 from the remote host.
"""
import http.client
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("SPARED_BIN", os.path.join(ROOT, "target/release/spared"))
STRICT_NET = os.environ.get("TEMPLATES_TEST_STRICT_NET") == "1"
SHIPPED_TOKEN = "change-me-before-you-deploy"
TOKEN = "blog-test-token-9f3a"


class Failure(Exception):
    pass


def expect(cond, what):
    if not cond:
        raise Failure(what)


def free_ports(n):
    found = []
    for port in range(19300, 19400):
        s = socket.socket()
        try:
            s.bind(("127.0.0.1", port))
        except OSError:
            continue
        finally:
            s.close()
        found.append(port)
        if len(found) == n:
            return found
    raise SystemExit("templates-test: no free ports in 19300-19399")


class Server:
    def __init__(self, tmp):
        self.tmp = tmp
        self.pub, self.adm = free_ports(2)
        self.home = os.path.join(tmp, "home")
        os.makedirs(self.home)
        self.env = dict(os.environ, HOME=self.home)
        # Keep finding the Rust toolchain after HOME changes.
        for var, default in (("RUSTUP_HOME", "~/.rustup"), ("CARGO_HOME", "~/.cargo")):
            self.env.setdefault(var, os.path.expanduser(default))
        self.log = open(os.path.join(tmp, "serve.log"), "w")
        # --experimental-wasip3 for the fetcher3 template, the other templates are wasip2 apps.
        self.proc = subprocess.Popen(
            [BIN, "serve", "--data-dir", os.path.join(tmp, "data"),
             "--listen", f"127.0.0.1:{self.pub}", "--admin-listen", f"127.0.0.1:{self.adm}",
             "--experimental-wasip3"],
            env=self.env, stdout=self.log, stderr=subprocess.STDOUT)
        token_file = os.path.join(tmp, "data", "admin-token")
        for _ in range(100):
            if self.proc.poll() is not None:
                raise SystemExit("templates-test: server exited during startup")
            if os.path.exists(token_file) and os.path.getsize(token_file) > 0:
                try:
                    c = http.client.HTTPConnection("127.0.0.1", self.adm, timeout=2)
                    c.request("GET", "/v1/apps", headers={"Authorization": "Bearer " + open(token_file).read().strip()})
                    if c.getresponse().status == 200:
                        self.token = open(token_file).read().strip()
                        return
                except OSError:
                    pass
            time.sleep(0.2)
        raise SystemExit("templates-test: server did not answer within 20 s")

    def stop(self):
        self.proc.terminate()
        try:
            self.proc.wait(10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        self.log.close()

    def cli_env(self):
        return dict(self.env, SPARED_URL=f"http://127.0.0.1:{self.adm}", SPARED_TOKEN=self.token)


def run(args, cwd, env, timeout=900):
    return subprocess.run([BIN] + args, cwd=cwd, env=env, capture_output=True, text=True, timeout=timeout)


class App:
    """Talks to a deployed app by Host header on the scratch server's public port. Never follows redirects."""

    def __init__(self, port, name):
        self.port, self.name = port, name

    def call(self, method, path, body=None, headers=None):
        c = http.client.HTTPConnection("127.0.0.1", self.port, timeout=30)
        h = {"Host": f"{self.name}.localhost:{self.port}"}
        h.update(headers or {})
        if isinstance(body, (dict, list)):
            body = json.dumps(body)
        c.request(method, path, body=body, headers=h)
        r = c.getresponse()
        data = r.read().decode("utf-8", "replace")
        return r.status, dict((k.lower(), v) for k, v in r.getheaders()), data

    def json(self, method, path, body=None, headers=None, want=200):
        status, _, data = self.call(method, path, body, headers)
        expect(status == want, f"{method} {path} answered {status}, want {want}: {data[:200]}")
        return json.loads(data)


def check_notes(a):
    n = a.json("POST", "/api/notes", {"text": "hello"}, want=201)
    expect(n["text"] == "hello", "created note echoes its text")
    expect([x["text"] for x in a.json("GET", "/api/notes")] == ["hello"], "note is listed")
    status, _, _ = a.call("DELETE", f"/api/notes/{n['id']}")
    expect(status == 200, "delete answers 200")
    expect(a.json("GET", "/api/notes") == [], "list is empty after delete")


def check_counter(a):
    expect(a.json("GET", "/api/count")["count"] == 0, "starts at 0")
    for want in (1, 2, 3):
        expect(a.json("POST", "/api/increment")["count"] == want, f"increment gives {want}")
    expect(a.json("GET", "/api/count")["count"] == 3, "count reads back 3")
    status, _, page = a.call("GET", "/")
    expect(status == 200 and "<button" in page, "page has a button")


def check_todo(a):
    t = a.json("POST", "/api/todos", {"text": "milk"}, want=201)
    expect(t["done"] is False, "new todo is not done")
    expect(a.json("PATCH", f"/api/todos/{t['id']}", {"done": True})["done"] is True, "patch ticks the box")
    expect(a.json("GET", "/api/todos")[0]["done"] is True, "done persists")
    status, _, _ = a.call("PATCH", "/api/todos/999", {"done": True})
    expect(status == 404, "patching a missing todo is 404")
    status, _, _ = a.call("DELETE", f"/api/todos/{t['id']}")
    expect(status == 200 and a.json("GET", "/api/todos") == [], "delete removes it")


def check_guestbook(a):
    a.json("POST", "/api/messages", {"name": "ann", "message": "first"}, want=201)
    a.json("POST", "/api/messages", {"name": "bob", "message": "<script>alert(1)</script>"}, want=201)
    status, _, page = a.call("GET", "/")
    expect(status == 200, "page answers 200")
    expect("<script>alert(1)</script>" not in page, "message HTML is not injected raw")
    expect("&lt;script&gt;alert(1)&lt;/script&gt;" in page, "message is HTML-escaped")
    expect(page.index("bob") < page.index("first"), "newest message comes first")
    status, _, _ = a.call("POST", "/api/messages", {"name": "", "message": ""})
    expect(status == 400, "empty message is refused")


def check_kv(a):
    for k, v in (("b", "2"), ("a", "1"), ("c", "3")):
        status, _, _ = a.call("PUT", f"/kv/{k}", v)
        expect(status == 200, f"PUT {k}")
    status, _, body = a.call("GET", "/kv/a")
    expect(status == 200 and body == "1", "GET returns the stored value")
    a.call("PUT", "/kv/a", "uno")
    expect(a.call("GET", "/kv/a")[2] == "uno", "PUT overwrites")
    expect([e["key"] for e in a.json("GET", "/kv")] == ["a", "b", "c"], "listing is sorted by key")
    expect(a.call("DELETE", "/kv/b")[0] == 200, "DELETE answers 200")
    expect(a.call("GET", "/kv/b")[0] == 404, "deleted key is 404")
    expect(a.call("PUT", "/kv/bad%2Fkey", "x")[0] == 400, "unsafe key is 400")


def check_shortener(a):
    link = a.json("POST", "/api/links", {"url": "https://example.com/long/path"}, want=201)
    code = link["code"]
    status, headers, _ = a.call("GET", f"/{code}")
    expect(status == 302 and headers.get("location") == "https://example.com/long/path", "short code redirects with 302")
    a.call("GET", f"/{code}")
    expect(a.json("GET", f"/api/links/{code}")["hits"] == 2, "two redirects count two hits")
    expect(a.call("GET", "/nosuch")[0] == 404, "unknown code is 404")
    status, _, _ = a.call("POST", "/api/links", {"url": "javascript:alert(1)"})
    expect(status == 400, "non-http URL is refused")


def check_webhook(a):
    good = {"x-webhook-secret": SHIPPED_TOKEN}
    status, _, _ = a.call("POST", "/hook", {"a": 1})
    expect(status == 401, "POST without the secret is 401")
    status, _, _ = a.call("POST", "/hook", {"a": 1}, {"x-webhook-secret": "wrong"})
    expect(status == 401, "POST with the wrong secret is 401")
    expect(a.call("GET", "/events")[0] == 401, "GET /events without the secret is 401")
    a.json("POST", "/hook", {"kind": "ping", "n": 1}, good, want=201)
    a.json("POST", "/hook", [1, 2, 3], good, want=201)
    for i in range(55):
        a.json("POST", "/hook", {"i": i}, good, want=201)
    events = a.json("GET", "/events", headers=good)
    expect(len(events) == 50, f"lists the last 50, got {len(events)}")
    expect(events[0]["payload"] == {"i": 54} and events[0]["received_at"].endswith("Z"), "newest first, with a timestamp")
    status, _, _ = a.call("POST", "/hook", "not json", good)
    expect(status == 400, "non-JSON body is 400")


def check_fetcher(a):
    info = a.json("GET", "/")
    expect("host" in info, "/ names the host")
    status, _, data = a.call("GET", "/fetch")
    body = json.loads(data)
    if status == 200:
        expect(isinstance(body["status"], int) and body["bytes"] > 0, "fetch reports status and size")
    else:
        expect(not STRICT_NET, f"strict network mode: /fetch answered {status}: {data[:200]}")
        expect(status == 502 and "error" in body, f"/fetch failed without the app's error JSON: {status} {data[:200]}")
        print("      note: no outbound network, /fetch returned the app's 502 error JSON", flush=True)


def check_fetcher3(a):
    info = a.json("GET", "/")
    expect(info.get("runtime") == "wasip3-experimental", f"/ names the runtime: {info}")
    check_fetcher(a)
    # Several requests at once on one instance: all of them answer.
    import concurrent.futures
    with concurrent.futures.ThreadPoolExecutor(4) as pool:
        statuses = list(pool.map(lambda _: a.call("GET", "/")[0], range(8)))
    expect(statuses == [200] * 8, f"8 requests at once answered {statuses}")


def check_blog_refuses_shipped_token(a):
    shipped = {"Authorization": "Bearer " + SHIPPED_TOKEN}
    for headers in ({}, shipped):
        status, _, body = a.call("POST", "/api/posts", {"title": "Hi", "body": "x"}, headers)
        expect(status == 503 and "change TOKEN" in body, f"publishing is refused while TOKEN is unchanged, got {status} {body[:80]}")
    status, _, index = a.call("GET", "/")
    expect(status == 200 and "posts/" not in index, "nothing was published with the shipped token")


def check_blog(a):
    status, _, _ = a.call("POST", "/api/posts", {"title": "Hi", "body": "x"})
    expect(status == 401, "publish without a token is 401")
    status, _, _ = a.call("POST", "/api/posts", {"title": "Hi", "body": "x"}, {"Authorization": "Bearer nope"})
    expect(status == 401, "publish with a wrong token is 401")
    auth = {"Authorization": "Bearer " + TOKEN}
    post = a.json("POST", "/api/posts", {"title": "Hello <World>", "body": "First <b>para</b>.\n\nSecond."}, auth, want=201)
    expect(post["slug"] == "hello-world", f"slug from title, got {post['slug']}")
    status, _, _ = a.call("POST", "/api/posts", {"title": "Hello <World>", "body": "again"}, auth)
    expect(status == 409, "duplicate slug is 409")
    status, _, index = a.call("GET", "/")
    expect(status == 200 and "posts/hello-world" in index and "Hello &lt;World&gt;" in index, "index lists the escaped title")
    status, _, page = a.call("GET", "/posts/hello-world")
    expect(status == 200 and "<p>Second.</p>" in page, "post page renders paragraphs")
    expect("<b>para</b>" not in page and "&lt;b&gt;para&lt;/b&gt;" in page, "post body is HTML-escaped")
    expect(a.call("GET", "/posts/missing")[0] == 404, "missing post is 404")


CHECKS = {
    "notes": (["/", "/api/notes"], check_notes),
    "counter": (["/", "/api/count"], check_counter),
    "todo": (["/", "/api/todos"], check_todo),
    "guestbook": (["/"], check_guestbook),
    "kv": (["/", "/kv"], check_kv),
    "shortener": (["/", "/api/links"], check_shortener),
    "webhook": (["/"], check_webhook),
    "fetcher": (["/"], check_fetcher),
    "fetcher3": (["/"], check_fetcher3),
    "blog": (["/"], check_blog),
}


def test_template(name, server, work):
    app_name = f"t-{name}"
    env = server.cli_env()
    r = run(["new", app_name, "--template", name], work, env)
    expect(r.returncode == 0, f"spared new failed: {r.stdout}{r.stderr}")
    app_dir = os.path.join(work, app_name)
    paths, check = CHECKS[name]
    args = ["test"] + [x for p in paths for x in ("--path", p)]
    r = run(args, app_dir, env)
    expect(r.returncode == 0, f"spared test failed:\n{r.stdout}{r.stderr}")
    expect("next: spared deploy" in r.stdout, "spared test prints the next step")
    r = run(["deploy"], app_dir, env)
    expect(r.returncode == 0, f"spared deploy failed:\n{r.stdout}{r.stderr}")
    if name == "blog":
        check_blog_refuses_shipped_token(App(server.pub, app_name))
        main_rs = os.path.join(app_dir, "src", "main.rs")
        with open(main_rs) as f:
            src = f.read()
        line = f'const TOKEN: &str = "{SHIPPED_TOKEN}";'
        expect(line in src, "the blog template no longer has the TOKEN line this test edits")
        with open(main_rs, "w") as f:
            f.write(src.replace(line, f'const TOKEN: &str = "{TOKEN}";'))
        r = run(["deploy"], app_dir, env)
        expect(r.returncode == 0, f"spared deploy of the edited blog failed:\n{r.stdout}{r.stderr}")
    check(App(server.pub, app_name))


def main():
    if not os.access(BIN, os.X_OK):
        raise SystemExit(f"templates-test: {BIN} not found; run cargo build --release -p spared")
    t0 = time.monotonic()
    tmp = tempfile.mkdtemp(prefix="spared-templates-")
    if "CARGO_TARGET_DIR" not in os.environ:
        os.environ["CARGO_TARGET_DIR"] = os.path.join(tmp, "target")
    server = None
    results = []
    try:
        listing = subprocess.run([BIN, "templates", "--json"], capture_output=True, text=True, check=True)
        names = [t["name"] for t in json.loads(listing.stdout)["templates"]]
        wanted = sys.argv[1:] or names
        work = os.path.join(tmp, "work")
        os.makedirs(work)
        server = Server(tmp)
        for name in wanted:
            t = time.monotonic()
            try:
                if name not in names:
                    raise Failure(f"'{name}' is not in `spared templates`")
                if name not in CHECKS:
                    raise Failure("no behavioural check for this template in scripts/templates-test.py")
                test_template(name, server, work)
                results.append((name, None, time.monotonic() - t))
                print(f"PASS {name} ({time.monotonic() - t:.1f}s)", flush=True)
            except Failure as e:
                results.append((name, str(e), time.monotonic() - t))
                print(f"FAIL {name} ({time.monotonic() - t:.1f}s): {e}", flush=True)
            except subprocess.TimeoutExpired as e:
                results.append((name, f"timeout: {e}", time.monotonic() - t))
                print(f"FAIL {name}: timeout", flush=True)
    finally:
        if server:
            server.stop()
        failed = any(r[1] for r in results)
        if failed or os.environ.get("KEEP_TMP") == "1":
            print(f"templates-test: kept {tmp}", file=sys.stderr)
        else:
            shutil.rmtree(tmp, ignore_errors=True)
    bad = [r[0] for r in results if r[1]]
    print(f"templates-test: {len(results) - len(bad)}/{len(results)} passed in {time.monotonic() - t0:.1f}s")
    if bad:
        print("templates-test: FAIL: " + ", ".join(bad), file=sys.stderr)
        sys.exit(1)
    if not results:
        sys.exit(1)


if __name__ == "__main__":
    main()
