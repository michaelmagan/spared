#!/usr/bin/env python3
"""Fresh-agent test acceptance checks: judge each deployed app from behaviour only. Usage: check.py N [port]"""
import json, sys, urllib.request, urllib.error, http.client

N = int(sys.argv[1]); PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8080
NAME = json.load(open(__file__.replace("check.py", "prompts.json")))[N - 1]["name"]
HOST = f"{NAME}.localhost"

def req(method, path, body=None, headers=None, follow=False):
    c = http.client.HTTPConnection("127.0.0.1", PORT, timeout=20)
    h = {"Host": HOST}
    data = None
    if body is not None:
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        h["content-type"] = "application/json"
    h.update(headers or {})
    c.request(method, path, data, h)
    r = c.getresponse(); raw = r.read()
    try: js = json.loads(raw)
    except Exception: js = None
    return r.status, js, raw.decode("utf8", "replace"), dict((k.lower(), v) for k, v in r.getheaders())

fails = []
def check(cond, msg):
    if not cond: fails.append(msg)

def page():
    s, _, t, h = req("GET", "/")
    check(s == 200 and "html" in h.get("content-type", "") and "<" in t, f"GET / is not an HTML 200 ({s})")
    return t

def run():
    if N == 1:
        page(); s, j, _, _ = req("GET", "/api/count"); check(s == 200 and j and "count" in j, "GET /api/count")
        c0 = j["count"]; s, j, _, _ = req("POST", "/api/increment"); check(j and j.get("count") == c0 + 1, "increment")
        s, j, _, _ = req("GET", "/api/count"); check(j and j.get("count") == c0 + 1, "count persisted")
    if N == 2:
        page(); s, j, _, _ = req("POST", "/api/todos", {"text": "buy milk"}); check(s == 201 and j and "id" in j, f"POST todo {s}")
        s, l, _, _ = req("GET", "/api/todos"); check(isinstance(l, list) and any(x.get("text") == "buy milk" for x in l), "todo listed")
        s, _, _, _ = req("DELETE", f"/api/todos/{j['id']}"); check(s == 200, f"DELETE {s}")
        s, l, _, _ = req("GET", "/api/todos"); check(not any(x.get("text") == "buy milk" for x in l), "todo deleted")
    if N == 3:
        s, j, _, _ = req("POST", "/api/entries", {"name": "ann", "message": "<b>x</b>"}); check(s == 201, f"POST entry {s}")
        req("POST", "/api/entries", {"name": "bob", "message": "second"})
        s, l, _, _ = req("GET", "/api/entries"); check(isinstance(l, list) and l[0].get("name") == "bob", "newest first")
        t = page(); check("<b>x</b>" not in t and ("&lt;b&gt;" in t), "HTML escaped in page")
    if N == 4:
        page(); s, j, _, _ = req("POST", "/api/links", {"url": "https://example.org/x"}); check(s == 201 and j and "code" in j, f"POST link {s}")
        s, _, _, h = req("GET", f"/r/{j['code']}"); check(s == 302 and h.get("location") == "https://example.org/x", f"redirect {s} {h.get('location')}")
        s, l, _, _ = req("GET", "/api/links"); check(isinstance(l, list) and any(x.get("code") == j["code"] and x.get("hits", 0) >= 1 for x in l), "hits counted")
    if N == 5:
        page(); s, j, _, _ = req("POST", "/api/polls", {"question": "q?", "options": ["a", "b"]}); check(s == 201 and j and "id" in j, f"POST poll {s}")
        i = j["id"]; s, _, _, _ = req("POST", f"/api/polls/{i}/vote", {"option": 1}); check(s == 200, f"vote {s}")
        s, j, _, _ = req("GET", f"/api/polls/{i}"); check(j and j["options"][1]["votes"] == 1 and j["options"][0]["votes"] == 0, "vote counted")
    if N == 6:
        page(); req("POST", "/api/notes", {"title": "Alpha", "body": "about cats"}); s, j, _, _ = req("POST", "/api/notes", {"title": "Beta", "body": "about DOGS"})
        check(s == 201 and j and "id" in j, f"POST note {s}")
        s, l, _, _ = req("GET", "/api/notes?q=dogs"); check(isinstance(l, list) and len(l) == 1 and l[0].get("title") == "Beta", "search")
        s, l, _, _ = req("GET", "/api/notes"); check(isinstance(l, list) and l[0].get("title") == "Beta", "newest first")
    if N == 7:
        page(); s, _, _, _ = req("PUT", "/api/kv/b", {"x": 1}); check(s == 200, f"PUT {s}")
        req("PUT", "/api/kv/a", [1, 2]); s, j, _, _ = req("GET", "/api/kv/b"); check(j == {"x": 1}, "GET value")
        s, l, _, _ = req("GET", "/api/kv"); check(l == ["a", "b"], f"keys {l}")
        s, _, _, _ = req("DELETE", "/api/kv/b"); check(s == 200, "DELETE"); s, j, _, _ = req("GET", "/api/kv/b"); check(s == 404, "404 after delete")
    if N == 8:
        s, j0, _, _ = req("GET", "/api/visits"); t0 = (j0 or {}).get("total", 0)
        req("GET", "/", headers={"user-agent": "agent-agent"}); req("GET", "/", headers={"user-agent": "agent-agent"})
        s, j, _, _ = req("GET", "/api/visits"); check(j and j["total"] == t0 + 2, f"total {j}")
        check(j and j["recent"] and j["recent"][0].get("user_agent") == "agent-agent", "recent has user_agent")
    if N == 9:
        page(); s, b, _, _ = req("GET", "/api/summary"); b = b if isinstance(b, dict) else {}
        t0 = b.get("total", 0); f0 = (b.get("by_category") or {}).get("food", 0); r0 = (b.get("by_category") or {}).get("rent", 0)
        s, _, _, _ = req("POST", "/api/expenses", {"amount": 12.5, "category": "food", "note": "lunch"}); check(s == 201, f"POST {s}")
        req("POST", "/api/expenses", {"amount": 7.5, "category": "food", "note": "snack"}); req("POST", "/api/expenses", {"amount": 30, "category": "rent", "note": "x"})
        s, j, _, _ = req("GET", "/api/summary")
        check(j and abs(j["total"] - t0 - 50) < 1e-6 and abs(j["by_category"]["food"] - f0 - 20) < 1e-6 and abs(j["by_category"]["rent"] - r0 - 30) < 1e-6, f"summary {j} (before: total {t0})")
    if N == 10:
        page(); s, j, _, _ = req("GET", "/api/probe"); check(s == 200 and j and j.get("status") == 200 and j.get("bytes", 0) > 100, f"probe {s} {j}")

try:
    run()
except Exception as e:
    fails.append(f"exception: {e!r}")
print(json.dumps({"n": N, "name": NAME, "pass": not fails, "failures": fails}))
sys.exit(1 if fails else 0)
