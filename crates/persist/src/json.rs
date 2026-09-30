//! Versioned JSON document store: atomic writes with a bounded backup of
//! the previous *valid* version, deterministic schema migration, and
//! quarantine-instead-of-crash on corrupt files. Used for the app's global
//! prefs and per-song `.editor.json` sidecars.

use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::{Path, PathBuf};

/// A schema-versioned JSON document. Implemented by each prefs struct;
/// migrations must be deterministic (same input bytes → same document).
pub trait Versioned {
    /// schema version this build writes
    const VERSION: u32;
    /// version recorded in an on-disk document (absent `version` = 0)
    fn version_of(doc: &serde_json::Value) -> u32 {
        doc.get("version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as u32
    }
    /// rewrite an older document into the current shape — called only when
    /// `0 <= version_of(doc) < VERSION`; should set `doc["version"]`
    fn migrate(_doc: &mut serde_json::Value) {}
    /// clamp/validate every persisted numeric/index value after load
    fn sanitize(&mut self) {}
}

/// Result of `load_json` — never a hard error: `value` is `None` only when
/// nothing valid could be recovered; `diagnostics` describe what happened
/// for the caller to surface non-fatally.
pub struct Load<T> {
    /// the recovered value, if any
    pub value: Option<T>,
    /// human-readable notes: corrupt-file quarantine, backup recovery,
    /// newer-version file
    pub diagnostics: Vec<String>,
    /// value came from the `.bak` backup of the previous valid version
    pub recovered_from_backup: bool,
}

/// `<path>` with the given tag appended after its extension:
/// `prefs.json` → `prefs.json.bak` / `prefs.json.corrupt`.
fn tagged_path(path: &Path, tag: &str) -> PathBuf {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => path.with_extension(format!("{ext}.{tag}")),
        None => path.with_extension(tag),
    }
}

/// the bounded backup of the previous valid version
pub fn backup_path(path: &Path) -> PathBuf {
    tagged_path(path, "bak")
}

fn quarantine_path(path: &Path) -> PathBuf {
    tagged_path(path, "corrupt")
}

fn decode<T: DeserializeOwned + Versioned>(
    bytes: &[u8],
    diag_path: &Path,
    diagnostics: &mut Vec<String>,
) -> Option<T> {
    let mut doc: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            diagnostics.push(format!("{}: invalid JSON ({e})", diag_path.display()));
            return None;
        }
    };
    let version = T::version_of(&doc);
    if version > T::VERSION {
        // a file from a newer build is still read — unknown fields are
        // ignored rather than destroying the newer data
        diagnostics.push(format!(
            "{}: schema version {version} is newer than this build reads ({})",
            diag_path.display(),
            T::VERSION
        ));
    } else if version < T::VERSION {
        T::migrate(&mut doc);
    }
    match serde_json::from_value::<T>(doc) {
        Ok(mut v) => {
            v.sanitize();
            Some(v)
        }
        Err(e) => {
            diagnostics.push(format!("{}: settings unusable ({e})", diag_path.display()));
            None
        }
    }
}

/// Load a versioned JSON document. A corrupt primary file is quarantined
/// to `<name>.corrupt` and the `.bak` backup is tried — the previous valid
/// settings survive an interrupted write.
pub fn load_json<T: DeserializeOwned + Versioned>(path: &Path) -> Load<T> {
    let mut diagnostics = Vec::new();
    if let Some(v) = std::fs::read(path)
        .ok()
        .and_then(|b| decode::<T>(&b, path, &mut diagnostics))
    {
        return Load {
            value: Some(v),
            diagnostics,
            recovered_from_backup: false,
        };
    }
    if !path.exists() {
        // missing file is a first run, not a failure
        return Load {
            value: None,
            diagnostics,
            recovered_from_backup: false,
        };
    }
    // corrupt: move the bad file aside so it can't break the next open
    let q = quarantine_path(path);
    match std::fs::rename(path, &q) {
        Ok(()) => diagnostics.push(format!("corrupt settings moved to {}", q.display())),
        Err(e) => diagnostics.push(format!(
            "corrupt settings at {} could not be quarantined ({e})",
            path.display()
        )),
    }
    let bak = backup_path(path);
    if let Some(v) = std::fs::read(&bak)
        .ok()
        .and_then(|b| decode::<T>(&b, &bak, &mut diagnostics))
    {
        diagnostics.push(format!("recovered settings from {}", bak.display()));
        return Load {
            value: Some(v),
            diagnostics,
            recovered_from_backup: true,
        };
    }
    Load {
        value: None,
        diagnostics,
        recovered_from_backup: false,
    }
}

/// Save a versioned JSON document: pretty JSON through `write_atomic`,
/// with the previous *valid* contents kept at `<name>.bak` (bounded — one
/// backup file; on Windows written atomically inside `ReplaceFileW`).
pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<PathBuf, crate::PersistError> {
    let text = serde_json::to_string_pretty(value).map_err(|e| {
        crate::PersistError::new(
            crate::Phase::Write,
            path,
            std::io::Error::new(std::io::ErrorKind::InvalidData, e),
        )
    })?;
    // only back up a file that is still well-formed — overwriting a good
    // .bak with corrupt bytes would destroy the recovery copy
    let backup = std::fs::read(path)
        .ok()
        .filter(|b| serde_json::from_slice::<serde_json::Value>(b).is_ok())
        .map(|_| backup_path(path));
    crate::write_atomic_opts(
        path,
        text.as_bytes(),
        &crate::AtomicOptions {
            backup: backup.as_deref(),
        },
    )
    .map(|()| backup.unwrap_or_else(|| backup_path(path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atomic::TEST_GUARD;

    #[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
    struct TestPrefs {
        version: u32,
        name: String,
        #[serde(default)]
        level: u32,
    }

    impl Default for TestPrefs {
        fn default() -> Self {
            Self {
                version: Self::VERSION,
                name: String::new(),
                level: 0,
            }
        }
    }

    impl Versioned for TestPrefs {
        const VERSION: u32 = 2;

        fn migrate(doc: &mut serde_json::Value) {
            // v0/v1 → v2: `title` was renamed `name`
            if let Some(t) = doc.get("title").and_then(|v| v.as_str()).map(String::from) {
                doc["name"] = t.into();
            }
            doc["version"] = 2.into();
        }

        fn sanitize(&mut self) {
            self.level = self.level.min(9);
        }
    }

    fn testdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("midi-editor-json-tests").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn load_missing_is_none_without_diagnostics() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let p = testdir("load_missing_is_none_without_diagnostics").join("x.json");
        let l: Load<TestPrefs> = load_json(&p);
        assert!(l.value.is_none());
        assert!(l.diagnostics.is_empty());
    }

    #[test]
    fn roundtrip_writes_version_and_recovers_backup() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("roundtrip_writes_version_and_recovers_backup");
        let p = dir.join("x.json");
        save_json(&p, &TestPrefs { version: 2, name: "a".into(), level: 3 }).unwrap();
        save_json(&p, &TestPrefs { version: 2, name: "b".into(), level: 4 }).unwrap();
        // the second save backed up the first — previous valid version
        let bak: TestPrefs =
            serde_json::from_slice(&std::fs::read(backup_path(&p)).unwrap()).unwrap();
        assert_eq!(bak.name, "a");
        // now corrupt the primary — load must quarantine and recover .bak
        std::fs::write(&p, b"{\"version\":2,\"name\":\"trunc").unwrap();
        let l: Load<TestPrefs> = load_json(&p);
        assert!(l.recovered_from_backup);
        assert_eq!(l.value.unwrap().name, "a");
        assert!(!p.exists(), "corrupt file quarantined away");
        assert!(quarantine_path(&p).exists());
    }

    #[test]
    fn corrupt_without_backup_returns_none_but_survives_open() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("corrupt_without_backup_returns_none_but_survives_open");
        let p = dir.join("x.json");
        std::fs::write(&p, b"not json {{{").unwrap();
        let l: Load<TestPrefs> = load_json(&p);
        assert!(l.value.is_none());
        assert!(!l.diagnostics.is_empty());
        assert!(!p.exists() && quarantine_path(&p).exists());
    }

    #[test]
    fn older_version_is_migrated_deterministically() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("older_version_is_migrated_deterministically");
        let p = dir.join("x.json");
        std::fs::write(&p, br#"{"title":"old","level":42}"#).unwrap();
        let a: Load<TestPrefs> = load_json(&p);
        let v = a.value.unwrap();
        assert_eq!(v.name, "old", "title migrated to name");
        assert_eq!(v.version, 2);
        assert_eq!(v.level, 9, "sanitize clamped out-of-range numeric");
    }

    #[test]
    fn newer_version_loads_known_fields_with_note() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("newer_version_loads_known_fields_with_note");
        let p = dir.join("x.json");
        std::fs::write(&p, br#"{"version":99,"name":"x","future_field":true}"#).unwrap();
        let l: Load<TestPrefs> = load_json(&p);
        assert_eq!(l.value.unwrap().name, "x");
        assert!(l.diagnostics.iter().any(|d| d.contains("newer")));
        // the file is NOT quarantined — it belongs to a newer build
        assert!(p.exists());
    }

    #[test]
    fn interrupted_write_recovers_previous_valid() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // simulate a torn temp-replace: primary is a truncated write, .bak
        // still holds the last good version
        let dir = testdir("interrupted_write_recovers_previous_valid");
        let p = dir.join("x.json");
        save_json(&p, &TestPrefs { version: 2, name: "good".into(), level: 1 }).unwrap();
        std::fs::copy(&p, backup_path(&p)).unwrap();
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.truncate(bytes.len() / 2);
        std::fs::write(&p, &bytes).unwrap();
        let l: Load<TestPrefs> = load_json(&p);
        assert!(l.recovered_from_backup);
        assert_eq!(l.value.unwrap().name, "good");
    }
}
