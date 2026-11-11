//! Records the git commit the binary was built from, for `spared --version`.
//!
//! `SPARED_GIT_SHA` in the environment wins (the Docker build has no `.git`); otherwise `git
//! rev-parse` is asked. With neither, the version is printed without a commit.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SPARED_GIT_SHA");
    let git_dir = std::path::Path::new("../../.git");
    for f in ["HEAD", "logs/HEAD"] {
        if git_dir.join(f).exists() {
            println!("cargo:rerun-if-changed=../../.git/{f}");
        }
    }
    let from_env = std::env::var("SPARED_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty());
    let sha = from_env.or_else(|| {
        let out = Command::new("git")
            .args(["rev-parse", "--short=12", "HEAD"])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    let sha: String = sha
        .unwrap_or_default()
        .chars()
        .filter(char::is_ascii_hexdigit)
        .take(12)
        .collect();
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let long = if sha.is_empty() {
        version
    } else {
        format!("{version} ({sha})")
    };
    println!("cargo:rustc-env=SPARED_VERSION_LONG={long}");
}
