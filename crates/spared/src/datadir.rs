//! The data directory's format stamp: `<data-dir>/format.json`.
//!
//! The stamp says which on-disk layout the directory uses, so a server that does not understand
//! the layout refuses to start instead of reading it wrongly. `docs/upgrading.md` states the rule.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The newest layout this build reads and writes. Raise it when a release changes what is under the
/// data dir in a way an older server would misread or ignore to the operator's cost (app directory
/// files, the component store, backups, per-app limits).
pub const DATA_FORMAT: u32 = 2;
/// What a directory is stamped with while no app has a limit an operator set: format 1 is what
/// every release since 0.0.1-preview reads, so going back to one keeps working.
pub const BASE_FORMAT: u32 = 1;
/// What a directory is stamped with while any app has a limit an operator set (`<data-dir>/limits/`).
/// A release that predates per-app limits would serve those apps without them, so it must refuse.
pub const LIMITS_FORMAT: u32 = 2;
/// The oldest layout this build can open (and bring up to `DATA_FORMAT`).
pub const OLDEST_READABLE_FORMAT: u32 = 1;
pub const STAMP_FILE: &str = "format.json";

#[derive(Debug)]
pub struct DataDirError {
    pub kind: &'static str,
    pub message: String,
    pub fix: String,
}

#[derive(Debug, PartialEq)]
pub struct Stamp {
    pub format: u32,
    pub created_by: String,
    pub last_opened_by: String,
}

fn parse(text: &str) -> Option<Stamp> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let s = |k: &str| v[k].as_str().unwrap_or("unknown").to_string();
    Some(Stamp {
        format: u32::try_from(v["format"].as_u64()?).ok()?,
        created_by: s("created_by"),
        last_opened_by: s("last_opened_by"),
    })
}

/// Decide whether a server that writes `current` and reads back to `oldest` may open a directory
/// stamped `found`.
fn judge(dir: &Path, found: &Stamp, current: u32, oldest: u32) -> Result<(), DataDirError> {
    let d = dir.display();
    if found.format > current {
        return Err(DataDirError {
            kind: "data_dir_too_new",
            message: format!(
                "{d} uses data format {} (last opened by spared {}); this spared ({}) reads up to format {current}. Nothing was changed",
                found.format,
                found.last_opened_by,
                crate::VERSION
            ),
            fix: format!(
                "run spared {} or newer on this data dir, or start this version with a different --data-dir (docs/upgrading.md)",
                found.last_opened_by
            ),
        });
    }
    if found.format < oldest {
        return Err(DataDirError {
            kind: "data_dir_too_old",
            message: format!(
                "{d} uses data format {} (last opened by spared {}); this spared ({}) reads {}. Nothing was changed",
                found.format,
                found.last_opened_by,
                crate::VERSION,
                if oldest == current {
                    format!("format {current} only")
                } else {
                    format!("formats {oldest} to {current}")
                }
            ),
            fix: "start it once with the newest spared release that still reads that format, which upgrades it in place, then start this one; the releases that read each format are listed in docs/upgrading.md".into(),
        });
    }
    Ok(())
}

/// Read the stamp without changing anything. `Ok(None)` means there is no stamp: a new directory,
/// or one from before stamps existed, which is format 1.
pub fn check(dir: &Path) -> Result<Option<Stamp>, DataDirError> {
    let file = dir.join(STAMP_FILE);
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(DataDirError {
                kind: "data_dir_stamp_unreadable",
                message: format!("cannot read {}: {e}", file.display()),
                fix: "make the file readable by the server's user".into(),
            });
        }
    };
    let Some(stamp) = parse(&text) else {
        return Err(DataDirError {
            kind: "data_dir_stamp_unreadable",
            message: format!(
                "{} is not a format stamp (expected JSON like {{\"format\":{DATA_FORMAT}}}). Nothing was changed",
                file.display()
            ),
            fix: format!(
                "restore the file from a copy of the data dir; if you are sure this spared ({}) last ran on it, delete the file and the server writes a new one",
                crate::VERSION
            ),
        });
    };
    judge(dir, &stamp, DATA_FORMAT, OLDEST_READABLE_FORMAT)?;
    Ok(Some(stamp))
}

/// Check the stamp, then write or refresh it with `format` (one this build reads): a directory
/// without one is stamped, and `last_opened_by` records this version. The directory must exist.
/// The server stamps [`LIMITS_FORMAT`] while any per-app limit is stored and [`BASE_FORMAT`]
/// otherwise, so only a directory that has limits is closed to releases that would ignore them.
pub fn stamp(dir: &Path, format: u32) -> Result<Stamp, DataDirError> {
    let found = check(dir)?;
    let me = crate::VERSION.to_string();
    let new = Stamp {
        format,
        created_by: found.as_ref().map_or(me.clone(), |s| s.created_by.clone()),
        last_opened_by: me,
    };
    if found.as_ref() == Some(&new) {
        return Ok(new);
    }
    let body = serde_json::json!({
        "format": new.format,
        "created_by": new.created_by,
        "last_opened_by": new.last_opened_by,
    });
    let (file, tmp) = (dir.join(STAMP_FILE), dir.join("format.json.tmp"));
    let write = || -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(format!("{body}\n").as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, &file)
    };
    write().map_err(|e| DataDirError {
        kind: "data_dir_stamp_unwritable",
        message: format!("cannot write {}: {e}", file.display()),
        fix: "make the data dir writable by the server's user and check free disk space".into(),
    })?;
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("spared-fmt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_dir_without_a_stamp_is_stamped_and_reopens() {
        let d = scratch("unstamped");
        std::fs::create_dir_all(d.join("apps/notes")).unwrap();
        assert_eq!(check(&d).unwrap(), None);
        let s = stamp(&d, BASE_FORMAT).unwrap();
        assert_eq!(s.format, BASE_FORMAT);
        assert_eq!(s.created_by, crate::VERSION);
        assert_eq!(check(&d).unwrap(), Some(s));
        let text = std::fs::read_to_string(d.join(STAMP_FILE)).unwrap();
        assert!(text.contains("\"format\":1"), "{text}");
        assert!(d.join("apps/notes").is_dir());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_newer_format_is_refused_and_left_alone() {
        let d = scratch("new");
        let text = r#"{"format":3,"created_by":"0.0.1","last_opened_by":"0.3.0"}"#;
        std::fs::write(d.join(STAMP_FILE), text).unwrap();
        for e in [check(&d).unwrap_err(), stamp(&d, BASE_FORMAT).unwrap_err()] {
            assert_eq!(e.kind, "data_dir_too_new");
            assert!(e.message.contains("format 3") && e.message.contains("0.3.0"));
            assert!(e.fix.contains("spared 0.3.0 or newer"), "{}", e.fix);
        }
        assert_eq!(std::fs::read_to_string(d.join(STAMP_FILE)).unwrap(), text);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn an_older_format_than_readable_is_refused() {
        let d = std::path::Path::new("/srv/data");
        let old = Stamp {
            format: 1,
            created_by: "0.0.1".into(),
            last_opened_by: "0.0.4".into(),
        };
        let e = judge(d, &old, 3, 2).unwrap_err();
        assert_eq!(e.kind, "data_dir_too_old");
        assert!(e.message.contains("formats 2 to 3"), "{}", e.message);
        assert!(e.fix.contains("docs/upgrading.md"));
        assert!(judge(d, &old, 3, 1).is_ok());
        assert!(judge(d, &old, 1, 1).is_ok());
    }

    #[test]
    fn a_stamp_that_does_not_parse_is_refused() {
        let d = scratch("bad");
        for text in ["", "not json", r#"{"format":"one"}"#, r#"{"format":-1}"#] {
            std::fs::write(d.join(STAMP_FILE), text).unwrap();
            assert_eq!(check(&d).unwrap_err().kind, "data_dir_stamp_unreadable");
        }
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn reopening_keeps_who_created_it() {
        let d = scratch("keep");
        let text = r#"{"format":1,"created_by":"0.0.0-older","last_opened_by":"0.0.0-older"}"#;
        std::fs::write(d.join(STAMP_FILE), text).unwrap();
        let s = stamp(&d, BASE_FORMAT).unwrap();
        assert_eq!(s.created_by, "0.0.0-older");
        assert_eq!(s.last_opened_by, crate::VERSION);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_format_goes_up_with_limits_and_back_down_without_them() {
        let d = scratch("limits");
        stamp(&d, BASE_FORMAT).unwrap();
        let s = stamp(&d, LIMITS_FORMAT).unwrap();
        assert_eq!(s.format, 2);
        assert_eq!(check(&d).unwrap().unwrap().format, 2);
        let text = std::fs::read_to_string(d.join(STAMP_FILE)).unwrap();
        assert!(text.contains("\"format\":2"), "{text}");
        let older = judge(&d, &check(&d).unwrap().unwrap(), 1, 1).unwrap_err();
        assert_eq!(
            older.kind, "data_dir_too_new",
            "a release that reads format 1 only refuses it"
        );
        assert_eq!(stamp(&d, BASE_FORMAT).unwrap().format, 1);
        assert!(judge(&d, &check(&d).unwrap().unwrap(), 1, 1).is_ok());
        std::fs::remove_dir_all(&d).unwrap();
    }
}
