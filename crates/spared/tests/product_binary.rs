//! What a stranger who downloads the binary gets. A normal build has none of the test-lab
//! commands or environment switches; they exist only with `--features lab`.

use std::process::{Command, Output};

const SPARED: &str = env!("CARGO_BIN_EXE_spared");
const LAB_COMMANDS: [&str; 6] = [
    "run-probe",
    "bench-wake",
    "bench-density",
    "escape-test",
    "bench-3b",
    "compile",
];
#[cfg(not(feature = "lab"))]
const LAB_SWITCHES: [&str; 3] = [
    "SPARED_NO_EPOCH",
    "SPARED_UNPINNED",
    "SPARED_PROBE_LEGACY_IO",
];

fn spared(args: &[&str]) -> Output {
    Command::new(SPARED)
        .args(args)
        .output()
        .expect("run the spared binary")
}

fn app_dir(tag: &str, spared_toml: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("spared-bin-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(
        d.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(d.join("spared.toml"), spared_toml).unwrap();
    d
}

fn spared_in(dir: &std::path::Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    Command::new(SPARED)
        .args(args)
        .current_dir(dir)
        .envs(envs.iter().copied())
        .output()
        .expect("run the spared binary")
}

#[test]
fn without_cargo_or_rustc_the_build_says_so_and_names_rustup() {
    let d = app_dir("nocargo", "name = \"x\"\n");
    let empty = d.join("empty-path");
    std::fs::create_dir_all(&empty).unwrap();
    let envs = [
        ("PATH", empty.to_str().unwrap()),
        ("HOME", d.to_str().unwrap()),
    ];
    let out = spared_in(&d, &["test"], &envs);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("error[no_cargo]: cannot run cargo"), "{err}");
    assert!(err.contains("rustup.rs"), "{err}");
    assert!(!err.contains("wasm32-wasip2"), "{err}");
    let out = spared_in(&d, &["--json", "test"], &envs);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json error");
    assert_eq!(v["error"]["kind"], "no_cargo");
    std::fs::remove_dir_all(&d).unwrap();
}

#[test]
fn a_fetch_entry_that_can_never_match_is_refused_before_building() {
    for (entry, why) in [
        ("https://example.com", "has a scheme"),
        ("*", "every host"),
        ("example.com:8443", "has a port"),
        ("127.0.0.1", "can never be granted"),
    ] {
        let d = app_dir(
            "fetch",
            &format!("name = \"x\"\n[grants]\nfetch = [{entry:?}]\n"),
        );
        for cmd in ["test", "deploy"] {
            let out = spared_in(&d, &[cmd], &[("HOME", d.to_str().unwrap())]);
            assert_eq!(out.status.code(), Some(1), "{cmd} {entry}");
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(
                err.contains("error[bad_manifest]: fetch entry"),
                "{cmd} {entry}: {err}"
            );
            assert!(
                err.contains(why) || err.contains("host names"),
                "{cmd} {entry}: {err}"
            );
        }
        std::fs::remove_dir_all(&d).unwrap();
    }
}

#[test]
fn deploy_help_describes_the_server_flag_and_its_env_twin() {
    let out = spared(&["deploy", "--help"]);
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("--server <URL>"), "{help}");
    assert!(
        help.contains("$SPARED_URL") && help.contains("8081"),
        "{help}"
    );
}

#[cfg(not(feature = "lab"))]
/// Whether the binary contains `needle`. `grep` instead of a search in Rust: a debug build is
/// hundreds of MB and a byte-window scan of it takes tens of seconds.
fn binary_contains(needle: &str) -> bool {
    Command::new("grep")
        .args(["-q", "-a", "-F", "--", needle, SPARED])
        .status()
        .expect("run grep")
        .success()
}

#[test]
fn help_lists_no_lab_command() {
    let out = spared(&["--help"]);
    assert!(out.status.success());
    let help = String::from_utf8_lossy(&out.stdout);
    for name in LAB_COMMANDS {
        assert!(
            !help.lines().any(|l| l.trim_start().starts_with(name)),
            "--help lists {name}"
        );
    }
    assert!(help.contains("serve"), "{help}");
}

#[cfg(not(feature = "lab"))]
mod default_build {
    use super::*;

    #[test]
    fn lab_commands_do_not_exist() {
        for name in LAB_COMMANDS {
            let out = spared(&[name, "--help"]);
            assert!(!out.status.success(), "{name} was accepted");
            let err = String::from_utf8_lossy(&out.stderr);
            assert!(err.contains("unrecognized subcommand"), "{name}: {err}");
        }
    }

    #[test]
    fn the_binary_carries_no_lab_switch() {
        for name in LAB_SWITCHES {
            assert!(!binary_contains(name), "the binary mentions {name}");
        }
    }
}

#[cfg(feature = "lab")]
mod lab_build {
    use super::*;

    #[test]
    fn lab_commands_exist_but_stay_hidden() {
        for name in LAB_COMMANDS {
            let out = spared(&[name, "--help"]);
            assert!(out.status.success(), "{name} --help failed");
        }
    }
}
