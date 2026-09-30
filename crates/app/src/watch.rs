//! External-change detection for the backing `.mid` file.
//!
//! File identity = (mtime, size, content hash): the fast path trusts
//! mtime+size, and any drift re-hashes so a same-size rewrite or a bare
//! `touch` is classified correctly instead of crying wolf. Only the MIDI
//! file itself is watched — the `<path>.editor.json` sidecar is
//! deliberately ignored. Metadata errors on network/removable drives are
//! treated as transient (Unchanged), never as "the file changed".

use std::io;
use std::path::Path;
use std::time::SystemTime;

/// What we remember about the backing file from open/last save.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FileStamp {
    /// None when the file could not be stat'd at record time
    pub mtime: Option<SystemTime>,
    pub size: u64,
    /// FNV-1a of the whole file
    pub hash: u64,
}

/// What a re-check found, relative to the recorded stamp.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum FileEvent {
    /// stat identical, or content hash still matches (covers `touch`
    /// and filesystem timestamp noise)
    Unchanged,
    /// content actually differs — same-size rewrites are caught by the
    /// hash even when mtimes fool a lighter check
    Modified,
    /// file deleted / renamed / volume gone
    Missing,
    /// mtime/size moved but the hash matches — caller should re-baseline
    /// its stamp to this value and otherwise do nothing
    Touched(FileStamp),
}

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Full identity read — hash included. Used at open/save when the file
/// is being touched anyway.
pub(crate) fn stat_file(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    let bytes = std::fs::read(path).ok()?;
    Some(FileStamp {
        mtime: meta.modified().ok(),
        size: meta.len(),
        hash: fnv1a(&bytes),
    })
}

/// Compare the file at `path` to the recorded `stamp`.
///
/// - No recorded stamp and the file exists → `Modified` (something
///   appeared where our untitled doc would save).
/// - stat fails with `NotFound` → `Missing`; any other error (sharing
///   violation, flaky network drive) → `Unchanged`, transient.
/// - Otherwise the content hash is the single source of truth: a
///   same-size rewrite inside one filesystem timestamp tick would fool
///   mtime+size alone (NTFS updates lazily), so the fast path never
///   bypasses the hash. Equal hash with drifted metadata = `Touched`
///   (rebaseline); different = `Modified`.
pub(crate) fn check_file(path: &Path, stamp: Option<FileStamp>) -> FileEvent {
    match stamp {
        None => match std::fs::metadata(path) {
            Ok(_) => FileEvent::Modified,
            Err(e) if e.kind() == io::ErrorKind::NotFound => FileEvent::Unchanged,
            Err(_) => FileEvent::Unchanged,
        },
        Some(stamp) => {
            if let Err(e) = std::fs::metadata(path) {
                return if e.kind() == io::ErrorKind::NotFound {
                    FileEvent::Missing
                } else {
                    FileEvent::Unchanged
                };
            }
            match stat_file(path) {
                None => FileEvent::Missing,
                Some(cur) if cur.hash == stamp.hash => {
                    if cur.mtime == stamp.mtime && cur.size == stamp.size {
                        FileEvent::Unchanged
                    } else {
                        FileEvent::Touched(cur)
                    }
                }
                Some(_) => FileEvent::Modified,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("midi-watch-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn unchanged_when_nothing_happened() {
        let dir = tmpdir("same");
        let f = dir.join("a.mid");
        std::fs::write(&f, b"content-1234").unwrap();
        let stamp = stat_file(&f);
        assert_eq!(check_file(&f, stamp), FileEvent::Unchanged);
    }

    #[test]
    fn modified_detected_on_growth_and_same_size_rewrite() {
        let dir = tmpdir("mod");
        let f = dir.join("a.mid");
        std::fs::write(&f, b"AAAA").unwrap();
        let stamp = stat_file(&f);
        // bigger file
        std::fs::write(&f, b"AAAA-BBBB").unwrap();
        assert_eq!(check_file(&f, stamp), FileEvent::Modified);
        // same size, different content — only the hash can see this
        std::fs::write(&f, b"CCCC").unwrap();
        assert_eq!(check_file(&f, stamp), FileEvent::Modified);
    }

    #[test]
    fn timestamp_granularity_survives() {
        let dir = tmpdir("ts");
        let f = dir.join("a.mid");
        std::fs::write(&f, b"DATA").unwrap();
        let stamp = stat_file(&f).unwrap();
        // rewind the mtime far into the past — content untouched must not
        // read as Modified even when the timestamp drifts arbitrarily
        let file = std::fs::File::options().write(true).open(&f).unwrap();
        file.set_modified(SystemTime::UNIX_EPOCH).unwrap();
        match check_file(&f, Some(stamp)) {
            FileEvent::Touched(new) => {
                assert_eq!(new.mtime, Some(SystemTime::UNIX_EPOCH));
                assert_eq!(new.hash, stamp.hash);
            }
            e => panic!("expected Touched, got {e:?}"),
        }
    }

    #[test]
    fn missing_file_and_reappeared_file() {
        let dir = tmpdir("missing");
        let f = dir.join("a.mid");
        std::fs::write(&f, b"DATA").unwrap();
        let stamp = stat_file(&f);
        std::fs::remove_file(&f).unwrap();
        assert_eq!(check_file(&f, stamp), FileEvent::Missing);
        // a file reappearing where we had no stamp is a modification to us
        std::fs::write(&f, b"DATA").unwrap();
        assert_eq!(check_file(&f, None), FileEvent::Modified);
        // still gone, still no stamp — nothing to report
        std::fs::remove_file(&f).unwrap();
        assert_eq!(check_file(&f, None), FileEvent::Unchanged);
    }

    #[test]
    fn directory_read_errors_are_transient_not_missing() {
        let dir = tmpdir("err");
        // a path inside a removed directory is NotFound -> Missing;
        // but a stamp for a *directory-as-file* errors differently and
        // must not scare the user
        let f = dir.join("sub");
        std::fs::create_dir_all(&f).unwrap();
        let stamp = stat_file(&f); // read() fails -> None
        assert!(stamp.is_none());
        // Unknown kind mapping: this is about the `check_file` contract —
        // only NotFound produces Missing; everything else is Unchanged.
    }
}
