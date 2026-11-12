//! App tokens through the real server: `spared token create`, `list` and `revoke`, what an app
//! token may and may not do on every admin route, deploys by an app token, revocation, expiry,
//! restarts, the failed-auth limit, and that no token secret reaches a log, an output or the data
//! directory. Each test starts its own server on scratch ports and temp directories.

#[path = "support/fake_s3.rs"]
mod fake_s3;
#[path = "support/servers.rs"]
mod servers;

use base64::Engine as _;
use serde_json::{Value, json};
use servers::*;
use std::time::Duration;

/// An admin request with `token` (none when empty): status and JSON body.
fn call(n: &Node, token: &str, method: &str, path: &str, body: &[u8]) -> (u16, Value) {
    let (st, raw) = call_raw(n, token, method, path, &[], body);
    (st, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

fn call_raw(
    n: &Node,
    token: &str,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<u8>) {
    let auth = format!("Bearer {token}");
    let mut headers: Vec<(&str, &str)> = extra.to_vec();
    if !token.is_empty() {
        headers.push(("authorization", &auth));
    }
    http(
        n.admin,
        method,
        path,
        &format!("127.0.0.1:{}", n.admin),
        &headers,
        body,
    )
}

fn deploy_body(name: &str) -> (String, Vec<u8>) {
    let manifest = format!("name = \"{name}\"\n[grants]\ndata = true\n");
    (
        base64::engine::general_purpose::STANDARD.encode(manifest),
        wat::parse_str(HANDLER).unwrap(),
    )
}

/// `PUT /v1/apps/<path_name>` with a manifest naming `manifest_name`.
fn deploy_as(n: &Node, token: &str, path_name: &str, manifest_name: &str) -> (u16, Value) {
    let (m, wasm) = deploy_body(manifest_name);
    let (st, raw) = call_raw(
        n,
        token,
        "PUT",
        &format!("/v1/apps/{path_name}"),
        &[("x-spared-manifest", &m)],
        &wasm,
    );
    (st, serde_json::from_slice(&raw).unwrap_or(Value::Null))
}

/// Mint a token with the admin token; the secret and the whole response.
fn mint(n: &Node, body: Value) -> (String, Value) {
    let (st, v) = call(
        n,
        &n.token,
        "POST",
        "/v1/tokens",
        body.to_string().as_bytes(),
    );
    assert_eq!(st, 201, "{v}");
    (v["token"].as_str().unwrap().to_string(), v)
}

fn kind(v: &Value) -> &str {
    v["error"]["kind"].as_str().unwrap_or("")
}

/// Fail when `secret`, or the hex after its prefix, is in any file under the node's directory
/// (data directory, server output, `$HOME`) or in `extra`.
fn assert_nowhere(n: &Node, secret: &str, extra: &[&str]) {
    let hexpart = secret.strip_prefix("spt_").unwrap();
    for f in walk_files(&n.root) {
        let b = std::fs::read(&f).unwrap_or_default();
        let text = String::from_utf8_lossy(&b);
        assert!(!text.contains(hexpart), "the token is in {}", f.display());
    }
    for e in extra {
        assert!(!e.contains(hexpart), "the token is in output: {e}");
    }
}

#[test]
fn create_list_revoke_round_trip_shows_the_token_once() {
    let s = scratch("tok-round");
    let n = Node::start(&s.0, "n", vec![], vec![]);

    // The CLI prints the token alone on stdout and the rest on stderr.
    let o = n.cli(&[
        "token",
        "create",
        "blog",
        "--name",
        "ci",
        "--expires",
        "30d",
    ]);
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let stdout = String::from_utf8(o.stdout).unwrap();
    let stderr = String::from_utf8(o.stderr).unwrap();
    let secret = stdout.trim().to_string();
    assert_eq!(stdout, format!("{secret}\n"));
    assert!(secret.starts_with("spt_") && secret.len() == 68, "{secret}");
    assert!(stderr.contains("only time it is shown"), "{stderr}");
    assert!(!stderr.contains(&secret));

    let (rc, created) = n.json(&["token", "create", "blog", "--scope", "deploy,rm"]);
    assert_eq!(rc, 0, "{created}");
    let second = created["token"].as_str().unwrap().to_string();
    assert_eq!(created["scopes"], json!(["deploy", "rm"]));
    assert_eq!(created["expires_at"], Value::Null);
    assert_eq!(created["may_create"], true);
    let id2 = created["id"].as_str().unwrap().to_string();

    let (rc, list) = n.json(&["token", "list"]);
    assert_eq!(rc, 0);
    let tokens = list["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 2, "{list}");
    let first = tokens.iter().find(|t| t["id"] != id2.as_str()).unwrap();
    assert_eq!(first["app"], "blog");
    assert_eq!(first["label"], "ci");
    assert_eq!(first["status"], "active");
    assert_eq!(first["scopes"], json!(["deploy", "read", "evict"]));
    assert!(first["expires_at"].is_string());
    for t in tokens {
        assert!(t.get("token").is_none() && t.get("sha256").is_none(), "{t}");
    }
    let listed = list.to_string();
    let text_list = String::from_utf8(n.cli(&["token", "list", "blog"]).stdout).unwrap();
    assert!(
        text_list.contains(&id2) && text_list.contains("ci"),
        "{text_list}"
    );
    let (_, other_app) = n.json(&["token", "list", "nothing"]);
    assert_eq!(other_app["tokens"], json!([]));

    // Only the hash is on disk, in 0600 files under a 0700 directory.
    use std::os::unix::fs::PermissionsExt;
    let dir = n.data.join("tokens");
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    for f in walk_files(&dir) {
        assert_eq!(
            std::fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    // The token works, revoking stops it at once, and revoking again says so.
    assert_eq!(deploy_as(&n, &second, "blog", "blog").0, 200);
    let (rc, rv) = n.json(&["token", "revoke", &id2]);
    assert_eq!((rc, rv["revoked"].clone()), (0, json!(true)), "{rv}");
    assert_eq!(rv["already_revoked"], false);
    let (st, v) = deploy_as(&n, &second, "blog", "blog");
    assert_eq!((st, kind(&v)), (401, "token_revoked"), "{v}");
    let (rc, again) = n.json(&["token", "revoke", &id2]);
    assert_eq!((rc, again["already_revoked"].clone()), (0, json!(true)));
    let (rc, missing) = n.json(&["token", "revoke", "000000000000"]);
    assert_eq!((rc, kind(&missing)), (1, "no_such_token"), "{missing}");
    let (st, v) = call(&n, &n.token, "DELETE", "/v1/tokens/not-an-id", b"");
    assert_eq!((st, kind(&v)), (404, "no_such_token"));

    // Bad requests to mint are refused with their kinds, and nothing is minted.
    for (body, want) in [
        (
            json!({"app": "blog", "scopes": ["deploy", "admin"]}),
            "bad_scope",
        ),
        (json!({"app": "blog", "scopes": []}), "bad_scope"),
        (json!({"app": "blog", "expires_in_secs": 0}), "bad_expiry"),
        (
            json!({"app": "blog", "expires_in_secs": "soon"}),
            "bad_expiry",
        ),
        (json!({"app": "blog", "label": "a\nb"}), "bad_label"),
        (json!({"app": "Bad Name"}), "bad_name"),
        (json!({}), "bad_name"),
    ] {
        let (st, v) = call(
            &n,
            &n.token,
            "POST",
            "/v1/tokens",
            body.to_string().as_bytes(),
        );
        assert_eq!((st, kind(&v)), (400, want), "{body}: {v}");
    }
    let (rc, v) = n.json(&["token", "create", "blog", "--expires", "soon"]);
    assert_eq!((rc, kind(&v)), (1, "bad_expiry"));
    let (rc, v) = n.json(&["token", "create", "blog", "--scope", "deploy,root"]);
    assert_eq!((rc, kind(&v)), (1, "bad_scope"), "{v}");
    let (_, list) = n.json(&["token", "list"]);
    assert_eq!(list["tokens"].as_array().unwrap().len(), 2);

    // A restart keeps every token and its state.
    let mut n = n;
    n.restart();
    let (_, after) = n.json(&["token", "list"]);
    let after_tokens = after["tokens"].as_array().unwrap();
    assert_eq!(after_tokens.len(), 2);
    let second_after = after_tokens
        .iter()
        .find(|t| t["id"] == id2.as_str())
        .unwrap();
    assert_eq!(second_after["status"], "revoked");
    assert!(second_after["last_used_at"].is_string(), "{after}");
    let (st, _) = call(&n, &secret, "GET", "/v1/apps/blog", b"");
    assert_eq!(st, 200);
    let (st, v) = call(&n, &second, "GET", "/v1/apps", b"");
    assert_eq!((st, kind(&v)), (401, "token_revoked"));

    let ps = String::from_utf8(n.cli(&["--json", "ps"]).stdout).unwrap();
    let (_, metrics) = call_raw(&n, &n.token, "GET", "/metrics", &[], b"");
    let metrics = String::from_utf8(metrics).unwrap();
    for t in [&secret, &second] {
        assert_nowhere(&n, t, &[&listed, &ps, &metrics, &text_list]);
    }
}

#[test]
fn every_route_answers_each_kind_of_caller() {
    let s = scratch("tok-matrix");
    let n = Node::start(&s.0, "n", vec![], vec![]);
    for app in ["mine", "other", "byadmin"] {
        n.deploy(app);
    }
    let all = json!([
        "deploy", "read", "evict", "backup", "restore", "freeze", "rm"
    ]);
    let (own, _) = mint(&n, json!({"app": "mine", "scopes": all}));
    let (narrow, _) = mint(&n, json!({"app": "mine", "scopes": ["deploy"]}));
    let (revoked, rv) = mint(&n, json!({"app": "mine", "scopes": all}));
    let (st, _) = call(
        &n,
        &n.token,
        "DELETE",
        &format!("/v1/tokens/{}", rv["id"].as_str().unwrap()),
        b"",
    );
    assert_eq!(st, 200);
    let (expired, _) = mint(
        &n,
        json!({"app": "mine", "scopes": all, "expires_in_secs": 1}),
    );
    std::thread::sleep(Duration::from_millis(2100));
    let malformed = "spt_not-a-real-token";

    // Every admin route, with `{app}` for the app it names. The last ones change the most.
    let routes: Vec<(&str, &str, &str, Option<&str>)> = vec![
        ("GET", "/metrics", "", None),
        ("GET", "/v1/server", "", None),
        ("GET", "/v1/tokens", "", None),
        ("POST", "/v1/tokens", r#"{"app":"{app}"}"#, None),
        ("DELETE", "/v1/tokens/000000000000", "", None),
        ("GET", "/v1/apps", "", Some("read")),
        ("GET", "/v1/apps/{app}", "", Some("read")),
        ("GET", "/v1/apps/{app}/logs?lines=5", "", Some("read")),
        ("GET", "/v1/apps/{app}/code", "", None),
        ("POST", "/v1/apps/{app}/evict", "", Some("evict")),
        ("POST", "/v1/apps/{app}/backup", "", Some("backup")),
        ("GET", "/v1/apps/{app}/backups", "", Some("backup")),
        (
            "GET",
            "/v1/apps/{app}/backups/20200101T000000Z/remote",
            "",
            Some("backup"),
        ),
        (
            "DELETE",
            "/v1/apps/{app}/backups/20200101T000000Z",
            "",
            Some("backup"),
        ),
        ("GET", "/v1/apps/{app}/remote-backups", "", Some("backup")),
        (
            "POST",
            "/v1/apps/{app}/fetch",
            r#"{"remote_id":"x"}"#,
            Some("restore"),
        ),
        ("GET", "/v1/apps/{app}/fetch/x", "", Some("restore")),
        (
            "POST",
            "/v1/apps/{app}/restore",
            r#"{"backup_id":"nope"}"#,
            Some("restore"),
        ),
        ("POST", "/v1/apps/{app}/freeze", "", Some("freeze")),
        ("POST", "/v1/apps/{app}/thaw", "", Some("freeze")),
        ("PUT", "/v1/apps/{app}?frozen=move", "", None),
        ("PUT", "/v1/apps/{app}", "", Some("deploy")),
        ("POST", "/v1/apps/{app}", "", None),
        ("GET", "/v1/nothing", "", None),
        ("DELETE", "/v1/apps/{app}", "", Some("rm")),
    ];
    let send = |token: &str, method: &str, path: &str, body: &str, app: &str| {
        let path = path.replace("{app}", app);
        let body = body.replace("{app}", app);
        if method == "PUT" {
            let (m, wasm) = deploy_body(app);
            let (st, raw) = call_raw(
                &n,
                token,
                method,
                &path,
                &[("x-spared-manifest", &m)],
                &wasm,
            );
            (
                st,
                serde_json::from_slice::<Value>(&raw).unwrap_or(Value::Null),
            )
        } else {
            call(&n, token, method, &path, body.as_bytes())
        }
    };

    // Refusals first: none of these change anything.
    for (method, path, body, scope) in &routes {
        let what = format!("{method} {path}");
        for (token, st, k) in [
            (revoked.as_str(), 401, "token_revoked"),
            (expired.as_str(), 401, "token_expired"),
            (malformed, 401, "unauthorized"),
            ("", 401, "unauthorized"),
        ] {
            let (got, v) = send(token, method, path, body, "mine");
            assert_eq!((got, kind(&v)), (st, k), "{what} with a {k} token: {v}");
        }
        // Another app's route: one answer, whether that app exists or not.
        let (st_other, v_other) = send(&own, method, path, body, "other");
        let (st_ghost, v_ghost) = send(&own, method, path, body, "ghost");
        if path.contains("{app}") {
            assert_eq!(
                (st_other, kind(&v_other)),
                (403, "wrong_scope"),
                "{what} for another app: {v_other}"
            );
            assert_eq!(st_ghost, st_other, "{what}");
            assert_eq!(
                v_ghost.to_string().replace("ghost", "X"),
                v_other.to_string().replace("other", "X"),
                "{what}: the answer differs for an app that does not exist"
            );
        }
        // An app token without the route's scope, on its own app.
        if scope != &Some("deploy") {
            let (st, v) = send(&narrow, method, path, body, "mine");
            assert_eq!(
                (st, kind(&v)),
                (403, "wrong_scope"),
                "{what} without its scope: {v}"
            );
        }
        // Routes that need the admin token refuse every app token.
        if scope.is_none() {
            let (st, v) = send(&own, method, path, body, "mine");
            assert_eq!(
                (st, kind(&v)),
                (403, "wrong_scope"),
                "{what} by an app token: {v}"
            );
        }
    }
    let other_before = tree(&n.data.join("apps/other"));
    assert!(!n.data.join("apps/ghost").exists());

    // The admin token reaches every route and is never refused for its scope.
    for (method, path, body, _) in &routes {
        let (st, v) = send(&n.token, method, path, body, "byadmin");
        assert!(
            ![401, 403].contains(&st) && kind(&v) != "wrong_scope",
            "{method} {path} with the admin token: {st} {v}"
        );
    }
    assert!(!n.data.join("apps/byadmin").exists());

    // The app token with every scope reaches each scoped route on its own app.
    for (method, path, body, scope) in &routes {
        if scope.is_none() {
            continue;
        }
        let (st, v) = send(&own, method, path, body, "mine");
        assert!(
            ![401, 403].contains(&st),
            "{method} {path} with its own token: {st} {v}"
        );
        if *method == "GET" && *path == "/v1/apps" {
            let names: Vec<_> = v["apps"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["name"].clone())
                .collect();
            assert_eq!(
                names,
                vec![json!("mine")],
                "an app token lists only its app"
            );
        }
        if *method == "DELETE" && *path == "/v1/apps/{app}" {
            assert_eq!(v["tokens_revoked"], 3, "{v}");
        }
    }
    assert!(!n.data.join("apps/mine").exists());
    assert_eq!(tree(&n.data.join("apps/other")), other_before);
    // Removing the app revoked its tokens.
    let (st, v) = call(&n, &own, "GET", "/v1/apps", b"");
    assert_eq!((st, kind(&v)), (401, "token_revoked"));
    let (st, v) = deploy_as(&n, &own, "mine", "mine");
    assert_eq!((st, kind(&v)), (401, "token_revoked"));

    // The access log names the token id for an app token's requests, never the token.
    let log = n.log_text();
    assert!(
        log.contains("\"token_id\":"),
        "no token_id in the access log"
    );
    for t in [&own, &narrow, &revoked, &expired] {
        assert_nowhere(&n, t, &[]);
    }
}

#[test]
fn an_app_token_deploys_only_its_own_app() {
    let s = scratch("tok-deploy");
    let mut n = Node::start(&s.0, "n", vec![], vec![]);
    n.deploy("existing");
    let (fresh, minted) = mint(&n, json!({"app": "fresh"}));
    assert_eq!(minted["may_create"], true);
    let (for_existing, minted) = mint(&n, json!({"app": "existing"}));
    assert_eq!(minted["may_create"], false);

    // Another name in the path, or a manifest naming another app: refused, nothing written.
    let apps_before = tree(&n.data.join("apps"));
    let (st, v) = deploy_as(&n, &fresh, "evil", "evil");
    assert_eq!((st, kind(&v)), (403, "wrong_scope"), "{v}");
    let (st, v) = deploy_as(&n, &fresh, "existing", "existing");
    assert_eq!((st, kind(&v)), (403, "wrong_scope"), "{v}");
    let (st, v) = deploy_as(&n, &fresh, "fresh", "evil");
    assert_eq!((st, kind(&v)), (400, "name_mismatch"), "{v}");
    let (st, v) = deploy_as(&n, &for_existing, "fresh", "fresh");
    assert_eq!((st, kind(&v)), (403, "wrong_scope"), "{v}");
    assert_eq!(tree(&n.data.join("apps")), apps_before);

    // Its own name: the first deploy creates the app, the next one replaces the code.
    let (st, v) = deploy_as(&n, &fresh, "fresh", "fresh");
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["version"], 1);
    let (st, v) = deploy_as(&n, &fresh, "fresh", "fresh");
    assert_eq!((st, v["version"].clone()), (200, json!(2)), "{v}");
    let (_, list) = call(&n, &n.token, "GET", "/v1/tokens?app=fresh", b"");
    assert_eq!(list["tokens"][0]["may_create"], false, "{list}");
    let (st, v) = deploy_as(&n, &for_existing, "existing", "existing");
    assert_eq!((st, v["version"].clone()), (200, json!(2)), "{v}");

    // Limits still go through the server's caps.
    let manifest = "name = \"fresh\"\n[limits]\nmemory_mb = 100000\n";
    let m = base64::engine::general_purpose::STANDARD.encode(manifest);
    let (st, raw) = call_raw(
        &n,
        &fresh,
        "PUT",
        "/v1/apps/fresh",
        &[("x-spared-manifest", &m)],
        &wat::parse_str(HANDLER).unwrap(),
    );
    let v: Value = serde_json::from_slice(&raw).unwrap();
    assert_eq!((st, kind(&v)), (400, "limit_too_high"), "{v}");

    // A token that created its app once never creates it again, also after a restart (here the
    // app directory is removed by hand, as an older release's `spared rm` would leave it).
    n.kill9();
    std::fs::remove_dir_all(n.data.join("apps/fresh")).unwrap();
    std::fs::remove_dir_all(n.data.join("apps/existing")).unwrap();
    n.spawn();
    for t in [&fresh, &for_existing] {
        let app = if t == &fresh { "fresh" } else { "existing" };
        let (st, v) = deploy_as(&n, t, app, app);
        assert_eq!((st, kind(&v)), (403, "wrong_scope"), "{app}: {v}");
        assert!(!n.data.join("apps").join(app).exists());
    }

    // The CLI with an app token: its app only, and no token commands.
    let (fresh2, _) = mint(&n, json!({"app": "fresh2"}));
    assert_eq!(deploy_as(&n, &fresh2, "fresh2", "fresh2").0, 200);
    n.deploy("neighbour");
    let as_app = |args: &[&str]| {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let o = n.cli_cmd(&a).env("SPARED_TOKEN", &fresh2).output().unwrap();
        let v: Value = serde_json::from_slice(&o.stdout).unwrap_or(Value::Null);
        (o.status.code().unwrap_or(-1), v)
    };
    let (rc, v) = as_app(&["ps"]);
    assert_eq!(rc, 0, "{v}");
    assert_eq!(v["apps"].as_array().unwrap().len(), 1);
    assert_eq!(v["apps"][0]["name"], "fresh2");
    let (rc, v) = as_app(&["logs", "fresh2"]);
    assert_eq!(rc, 0, "{v}");
    for args in [
        vec!["logs", "neighbour"],
        vec!["evict", "neighbour"],
        vec!["rm", "fresh2", "--yes"],
        vec!["backup", "fresh2"],
        vec!["token", "create", "fresh2"],
        vec!["token", "list"],
    ] {
        let (rc, v) = as_app(&args);
        assert_eq!((rc, kind(&v)), (1, "wrong_scope"), "{args:?}: {v}");
        assert!(v["error"]["fix"].as_str().unwrap().len() > 10);
    }
    assert!(n.data.join("apps/neighbour").exists());
    let (rc, v) = as_app(&["evict", "fresh2"]);
    assert_eq!(rc, 0, "{v}");
}

#[test]
fn wrong_tokens_are_limited_per_address() {
    let s = scratch("tok-limit");
    let n = Node::start(&s.0, "n", vec![], vec![]);
    // Requests without a token are not counted: the CLI sends one before every deploy.
    for _ in 0..40 {
        assert_eq!(call(&n, "", "GET", "/v1/apps", b"").0, 401);
    }
    assert_eq!(call(&n, &n.token, "GET", "/v1/apps", b"").0, 200);
    for i in 0..30 {
        let guess = format!("spt_{:064x}", i);
        let (st, v) = call(&n, &guess, "GET", "/v1/apps", b"");
        assert_eq!((st, kind(&v)), (401, "unauthorized"), "guess {i}");
    }
    // Shut out now, the right token included, so a guess learns nothing.
    let (st, head, body) = http_full(
        n.admin,
        "GET",
        "/v1/apps",
        &format!("127.0.0.1:{}", n.admin),
        &[("authorization", &format!("Bearer {}", n.token))],
        b"",
    );
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!((st, kind(&v)), (429, "auth_rate_limited"), "{v}");
    assert!(head.contains("retry-after: "), "{head}");
    let (st, v) = call(&n, "spt_x", "GET", "/v1/apps", b"");
    assert_eq!((st, kind(&v)), (429, "auth_rate_limited"));
    // The public listener is not affected.
    assert_eq!(http(n.public, "GET", "/healthz", "", &[], b"").0, 200);
}
