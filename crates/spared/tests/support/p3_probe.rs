//! The wasip3 test guest in `probes/p3-probe`, built once per test process with the repo's
//! toolchain for wasm32-wasip2. A missing wasm32-wasip2 target fails the test with the fix: the
//! tests of `runtime = "wasip3-experimental"` never pass without running the guest.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::OnceLock;

pub fn probe_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../probes/p3-probe")
}

/// The probe's component bytes.
pub fn p3_probe() -> Vec<u8> {
    static WASM: OnceLock<Vec<u8>> = OnceLock::new();
    WASM.get_or_init(|| {
        let dir = probe_dir();
        let target = dir.join("target");
        let out = std::process::Command::new(std::env::var("CARGO").unwrap_or("cargo".into()))
            .args(["build", "--release", "--locked", "--target", "wasm32-wasip2"])
            .current_dir(&dir)
            .env("CARGO_TARGET_DIR", &target)
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .output()
            .expect("run cargo to build probes/p3-probe");
        assert!(
            out.status.success(),
            "building probes/p3-probe failed (needs the wasm32-wasip2 target: `rustup target add wasm32-wasip2`):\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        std::fs::read(target.join("wasm32-wasip2/release/p3_probe.wasm"))
            .expect("probes/p3-probe built no p3_probe.wasm")
    })
    .clone()
}
