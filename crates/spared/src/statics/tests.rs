use super::*;
use crate::remote::archive::tests::scratch;
use std::io::Write as _;

fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.insert(
            header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    h
}

fn archive_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = archive::FileTarWriter::new(Vec::new()).unwrap();
    for (n, d) in files {
        w.add(n, d).unwrap();
    }
    w.finish().unwrap()
}

fn small() -> Limits {
    Limits {
        max_bytes: 1 << 20,
        max_files: 8,
    }
}

#[test]
fn names_are_relative_visible_and_plain() {
    for good in [
        "index.html",
        "assets/app.3f9a8b2c.js",
        "a b/c%20d.txt",
        "ünïcode/файл.css",
        "index.html.",
    ] {
        assert!(valid_name(good), "{good}");
    }
    let long = "a".repeat(MAX_NAME_BYTES + 1);
    let deep = vec!["d"; MAX_DEPTH + 1].join("/");
    for bad in [
        "",
        "/etc/passwd",
        "a/",
        "a//b",
        ".",
        "..",
        "../x",
        "a/../b",
        "a/./b",
        ".git/config",
        "a/.env",
        "a\\b",
        "a\0b",
        "a\nb",
        "a\u{7f}b",
        "a\u{85}b",
        long.as_str(),
        deep.as_str(),
    ] {
        assert!(!valid_name(bad), "{bad:?}");
    }
}

#[test]
fn request_paths_decode_once_and_never_name_anything_outside() {
    assert_eq!(lookup_key("/").as_deref(), Some("index.html"));
    assert_eq!(lookup_key("/docs/").as_deref(), Some("docs/index.html"));
    assert_eq!(lookup_key("/app.js").as_deref(), Some("app.js"));
    assert_eq!(lookup_key("/a%20b.txt").as_deref(), Some("a b.txt"));
    assert_eq!(lookup_key("/a%2Fb").as_deref(), Some("a/b"));
    // Decoded once: a doubly encoded dot pair is the literal name "%2e%2e".
    assert_eq!(lookup_key("/%252e%252e/x").as_deref(), Some("%2e%2e/x"));
    let long = format!("/{}", "a".repeat(MAX_RAW_PATH));
    for bad in [
        "",
        "index.html",
        "//",
        "//etc/passwd",
        "/a//b",
        "/..",
        "/../x",
        "/a/../b",
        "/a/./b",
        "/%2e%2e/x",
        "/%2E%2E/x",
        "/.%2e/x",
        "/%2e",
        "/%2F",
        "/%2F%2Fx",
        "/a%5cb",
        "/a\\b",
        "/a%00b",
        "/a%0ab",
        "/.git/config",
        "/%2egit/config",
        "/%zz",
        "/%4",
        "/%c3%28",
        "/x/..%2f..%2fsecret",
        long.as_str(),
    ] {
        assert_eq!(lookup_key(bad), None, "{bad:?}");
    }
}

#[test]
fn content_types_come_from_a_small_table() {
    for (name, want) in [
        ("index.html", "text/html; charset=utf-8"),
        ("A/B.HTML", "text/html; charset=utf-8"),
        ("s.css", "text/css; charset=utf-8"),
        ("m.mjs", "text/javascript; charset=utf-8"),
        ("app.js", "text/javascript; charset=utf-8"),
        ("app.js.map", "application/json"),
        ("i.svg", "image/svg+xml"),
        ("f.woff2", "font/woff2"),
        ("site.webmanifest", "application/manifest+json"),
        ("m.wasm", "application/wasm"),
        ("README", "application/octet-stream"),
        ("x.exe", "application/octet-stream"),
        ("dir.js/file", "application/octet-stream"),
    ] {
        assert_eq!(content_type(name), want, "{name}");
    }
}

#[test]
fn only_names_with_a_content_hash_are_immutable() {
    for yes in [
        "app.3f9a8b2c.js",
        "assets/chunk-1d2e3f4a.css",
        "x_0123456789abcdef.woff2",
    ] {
        assert!(immutable(yes), "{yes}");
    }
    for no in [
        "index.html",
        "app.js",
        "app.12345678.js",
        "app.deadbeef.js",
        "app.3f9a8b2.js",
        "app.3F9A8B2C.js",
        "3f9a8b2c/app.js",
        "index-BXyZ12ab.js",
    ] {
        assert!(!immutable(no), "{no}");
    }
}

#[test]
fn a_project_directory_packs_and_unpacks_into_hashed_files() {
    let s = scratch("static-pack");
    let src = s.0.join("static");
    std::fs::create_dir_all(src.join("assets/img")).unwrap();
    std::fs::create_dir_all(src.join(".git")).unwrap();
    std::fs::write(src.join("index.html"), b"<h1>hi</h1>").unwrap();
    std::fs::write(src.join("assets/app.js"), b"same").unwrap();
    std::fs::write(src.join("assets/img/copy.js"), b"same").unwrap();
    std::fs::write(src.join("assets/empty.txt"), b"").unwrap();
    std::fs::write(src.join(".git/config"), b"secret").unwrap();
    std::fs::write(src.join(".DS_Store"), b"x").unwrap();
    let p = pack(&src, "static").unwrap();
    let names: Vec<&str> = p.files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "assets/app.js",
            "assets/empty.txt",
            "assets/img/copy.js",
            "index.html"
        ]
    );
    assert_eq!(p.bytes, 19);
    assert_eq!(p.skipped, [".DS_Store", ".git/"]);

    let dest = s.0.join("static-1");
    let site = unpack(&p.archive, &dest, &small()).unwrap();
    assert_eq!((site.file_count(), site.bytes()), (4, 19));
    let e = site.get("index.html").unwrap().clone();
    assert_eq!(e.sha256, archive::hex(&Sha256::digest(b"<h1>hi</h1>")));
    assert_eq!(&site.read(&e).unwrap()[..], b"<h1>hi</h1>");
    // One file per distinct content, named by its hash, plus the index; nothing named after the upload.
    let mut on_disk: Vec<String> = std::fs::read_dir(&dest)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    on_disk.sort();
    assert_eq!(on_disk.len(), 4);
    assert!(on_disk.contains(&INDEX.to_string()));
    assert!(on_disk.iter().filter(|n| *n != INDEX).all(|n| is_sha256_hex(n)));
    let again = Site::load(&dest).unwrap();
    assert_eq!(again.get("assets/img/copy.js").unwrap().sha256, again.get("assets/app.js").unwrap().sha256);
    assert!(site.get(".git/config").is_none());
}

#[test]
fn packing_refuses_links_special_files_bad_names_and_empty_or_missing_dirs() {
    let s = scratch("static-pack-bad");
    let src = s.0.join("static");
    let e = pack(&src, "static").unwrap_err();
    assert_eq!((e.kind, e.message.contains("does not exist")), ("no_static_files", true));
    std::fs::create_dir_all(src.join(".hidden")).unwrap();
    std::fs::write(src.join(".hidden/x"), b"x").unwrap();
    let e = pack(&src, "static").unwrap_err();
    assert_eq!((e.kind, e.message.contains("no files")), ("no_static_files", true));

    std::fs::write(src.join("ok.txt"), b"x").unwrap();
    std::os::unix::fs::symlink("/etc/passwd", src.join("passwd")).unwrap();
    let e = pack(&src, "static").unwrap_err();
    assert_eq!((e.kind, e.message.contains("symlink")), ("static_bad_path", true));
    std::fs::remove_file(src.join("passwd")).unwrap();

    let fifo = std::ffi::CString::new(src.join("pipe").as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let e = pack(&src, "static").unwrap_err();
    assert_eq!((e.kind, e.message.contains("not a regular file")), ("static_bad_path", true));
    std::fs::remove_file(src.join("pipe")).unwrap();

    std::fs::write(src.join("back\\slash"), b"x").unwrap();
    assert_eq!(pack(&src, "static").unwrap_err().kind, "static_bad_path");
    std::fs::remove_file(src.join("back\\slash")).unwrap();

    let raw = std::ffi::OsStr::from_bytes(b"bad\xffname");
    std::fs::write(src.join(raw), b"x").unwrap();
    let e = pack(&src, "static").unwrap_err();
    assert_eq!((e.kind, e.message.contains("UTF-8")), ("static_bad_path", true));
    std::fs::remove_file(src.join(raw)).unwrap();

    let mut big = File::create(src.join("big.bin")).unwrap();
    big.write_all(&vec![1u8; MAX_FILE_BYTES as usize + 1]).unwrap();
    assert_eq!(pack(&src, "static").unwrap_err().kind, "static_too_large");
}

#[test]
fn hostile_uploads_are_refused_by_kind() {
    let s = scratch("static-hostile");
    let mut n = 0;
    let mut try_unpack = |archive: &[u8], limits: &Limits| {
        n += 1;
        let dest = s.0.join(format!("static-{n}"));
        unpack(archive, &dest, limits).map(|_| ()).map_err(|e| (e.kind, e.message))
    };
    for name in ["../escape", "/etc/passwd", "a//b", ".git/config", "a\\b", "a/./b", "x\u{1b}y"] {
        let e = try_unpack(&archive_of(&[(name, b"x")]), &small()).unwrap_err();
        assert_eq!(e.0, "static_bad_path", "{name:?}: {}", e.1);
    }
    let e = try_unpack(&archive_of(&[("a", b"1"), ("a", b"2")]), &small()).unwrap_err();
    assert_eq!(e.0, "static_bad_path");
    assert!(e.1.contains("twice"), "{}", e.1);

    let many: Vec<(String, Vec<u8>)> = (0..9).map(|i| (format!("f{i}"), vec![b'x'])).collect();
    let refs: Vec<(&str, &[u8])> = many.iter().map(|(a, b)| (a.as_str(), b.as_slice())).collect();
    let e = try_unpack(&archive_of(&refs), &small()).unwrap_err();
    assert_eq!(e.0, "static_too_large");
    assert!(e.1.contains("--max-static-files"), "{}", e.1);

    let big = vec![7u8; 600 << 10];
    let e = try_unpack(&archive_of(&[("a", &big), ("b", &big)]), &small()).unwrap_err();
    assert_eq!(e.0, "static_too_large");
    assert!(e.1.contains("--max-static-mb"), "{}", e.1);

    let e = try_unpack(b"<h1>not an archive</h1>", &small()).unwrap_err();
    assert_eq!(e.0, "static_bad_archive");

    // A bomb: a valid tar whose compressed stream then runs on with zeros far past the cap.
    let mut z = zstd::stream::write::Encoder::new(Vec::new(), 19).unwrap();
    let tar = {
        let plain = archive_of(&[("index.html", b"hi")]);
        zstd::stream::decode_all(&plain[..]).unwrap()
    };
    z.write_all(&tar).unwrap();
    z.write_all(&vec![0u8; 64 << 20]).unwrap();
    let bomb = z.finish().unwrap();
    assert!(bomb.len() < 64 << 10, "{}", bomb.len());
    let e = try_unpack(&bomb, &small()).unwrap_err();
    assert_eq!(e.0, "static_too_large", "{}", e.1);

    // Not refused: the same files within the caps.
    assert!(try_unpack(&archive_of(&[("index.html", b"hi")]), &small()).is_ok());
}

fn entry(data: &[u8]) -> Entry {
    Entry {
        sha256: archive::hex(&Sha256::digest(data)),
        size: data.len() as u64,
    }
}

#[test]
fn conditional_and_range_requests_are_planned_from_the_headers() {
    let e = entry(b"0123456789");
    let tag = etag(&e);
    assert_eq!(plan(&e, false, &hm(&[])), Plan::Whole);
    for inm in [tag.clone(), format!("W/{tag}"), format!("\"x\", {tag}"), "*".into()] {
        assert_eq!(plan(&e, false, &hm(&[("if-none-match", &inm)])), Plan::NotModified, "{inm}");
        assert_eq!(plan(&e, true, &hm(&[("if-none-match", &inm)])), Plan::NotModified, "{inm}");
    }
    assert_eq!(plan(&e, false, &hm(&[("if-none-match", "\"other\"")])), Plan::Whole);
    for (range, want) in [
        ("bytes=0-3", Plan::Part(0, 3)),
        ("bytes=2-", Plan::Part(2, 9)),
        ("bytes=-3", Plan::Part(7, 9)),
        ("bytes=-30", Plan::Part(0, 9)),
        ("bytes=5-500", Plan::Part(5, 9)),
        ("bytes=10-", Plan::Unsatisfiable),
        ("bytes=-0", Plan::Unsatisfiable),
        ("bytes=0-1,4-5", Plan::Whole),
        ("bytes=5-2", Plan::Whole),
        ("bytes=x-2", Plan::Whole),
        ("items=0-1", Plan::Whole),
        ("bytes=-", Plan::Whole),
    ] {
        assert_eq!(plan(&e, false, &hm(&[("range", range)])), want, "{range}");
    }
    assert_eq!(plan(&e, true, &hm(&[("range", "bytes=0-3")])), Plan::Whole, "HEAD ignores Range");
    assert_eq!(
        plan(&e, false, &hm(&[("range", "bytes=0-3"), ("if-range", &tag)])),
        Plan::Part(0, 3)
    );
    assert_eq!(
        plan(&e, false, &hm(&[("range", "bytes=0-3"), ("if-range", "\"old\"")])),
        Plan::Whole
    );
}

#[test]
fn responses_carry_type_etag_cache_and_length() {
    let data = Bytes::from_static(b"0123456789");
    let e = entry(&data);
    let r = response("app.js", &e, Plan::Whole, false, Some(data.clone()));
    let h = r.headers();
    assert_eq!(r.status(), 200);
    assert_eq!(h["content-type"], "text/javascript; charset=utf-8");
    assert_eq!(h["etag"], etag(&e).as_str());
    assert_eq!(h["cache-control"], "no-cache");
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["accept-ranges"], "bytes");
    let r = response("app.3f9a8b2c.js", &e, Plan::Part(2, 4), false, Some(data.clone()));
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 2-4/10");
    assert_eq!(r.headers()["cache-control"], "public, max-age=31536000, immutable");
    let r = response("a.txt", &e, Plan::Whole, true, None);
    assert_eq!(r.headers()["content-length"], "10");
    let r = response("a.txt", &e, Plan::NotModified, false, None);
    assert_eq!(r.status(), 304);
    assert!(r.headers().get("content-type").is_none());
    let r = response("a.txt", &e, Plan::Unsatisfiable, false, None);
    assert_eq!((r.status().as_u16(), &r.headers()["content-range"]), (416, "bytes */10"));
}

#[test]
fn large_files_are_mapped_and_a_removed_release_reads_as_not_found() {
    let s = scratch("static-read");
    let big: Vec<u8> = (0..(MAP_ABOVE as usize * 3)).map(|i| (i % 251) as u8).collect();
    let dest = s.0.join("static-1");
    let site = unpack(&archive_of(&[("big.bin", &big), ("small.txt", b"s")]), &dest, &small()).unwrap();
    let e = site.get("big.bin").unwrap().clone();
    let b = site.read(&e).unwrap();
    assert_eq!(&b[..], &big[..]);
    let part = b.slice(5..10);
    drop(b);
    assert_eq!(&part[..], &big[5..10]);
    crate::backup::remove_tree(&dest).unwrap();
    let err = site.read(site.get("small.txt").unwrap()).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    assert_eq!(&part[..], &big[5..10], "a mapping outlives the unlinked file");
}

#[test]
fn a_tampered_index_or_pointer_is_refused_at_load() {
    let s = scratch("static-load");
    let dest = s.0.join("static-7");
    unpack(&archive_of(&[("a.txt", b"a")]), &dest, &small()).unwrap();
    std::fs::write(s.0.join(POINTER), "static-7\n").unwrap();
    assert_eq!(load_current(&s.0).unwrap().unwrap().file_count(), 1);
    for bad in ["../../etc", "static-7/../x", "data", "static-"] {
        std::fs::write(s.0.join(POINTER), bad).unwrap();
        assert!(load_current(&s.0).is_err(), "{bad}");
    }
    std::fs::write(s.0.join(POINTER), "").unwrap();
    assert!(load_current(&s.0).unwrap().is_none());
    let index = dest.join(INDEX);
    let good = std::fs::read_to_string(&index).unwrap();
    std::fs::remove_file(&index).unwrap();
    std::fs::write(&index, good.replace("a.txt", "../a.txt")).unwrap();
    assert!(Site::load(&dest).is_err());
    std::fs::remove_file(&index).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", &index).unwrap();
    assert!(Site::load(&dest).is_err(), "the index is never read through a link");
}
