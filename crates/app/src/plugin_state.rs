//! Per-song VST3 state store — the binary companion to `song.mid.editor.json`.
//!
//! `Plugin::save_state` produces an opaque component+controller blob that can
//! be megabytes on instrument plugins, so it does not belong in the JSON
//! sidecar. This file lives beside it as `song.mid.editor.state`: a tiny
//! versioned container of length-prefixed records — a UTF-8 identity key, a
//! small JSON metadata header (path/class/vendor/name/version + when it was
//! captured), then the raw state blob.
//!
//! Records are keyed by a *stable plugin identity*: the VST3 class/component
//! uid when it is known (probe-scanned or loaded plugins), the bundle path
//! otherwise. A record is only replaced when the same plugin's state is
//! captured again — a plugin that is temporarily missing keeps its record, so
//! state survives rescans, unplugged installs, and catalog churn. A record
//! keyed by path is promoted to its uid key on the next capture once the uid
//! is known; lookup tries uid first and falls back to the path key so older
//! records still resolve.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 8] = b"MEDPLGST";
const FORMAT_VERSION: u32 = 1;
/// Sanity cap on one record's metadata header (bytes of UTF-8 JSON) — the
/// fields it carries are small; a larger header means the file isn't ours.
const MAX_META_LEN: usize = 1 << 16;
/// Sanity cap on one record's state blob — prevents a corrupt length prefix
/// from allocating the whole file (or disk) at once.
const MAX_BLOB_LEN: u64 = 1 << 30;

/// Companion-file path for a document: `<song>.mid.editor.state`.
pub fn state_path(doc_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.editor.state", doc_path.display()))
}

/// Identity key for a plugin: its class/component uid when known, else the
/// bundle path it was captured from (`path:` prefix keeps it disjoint from
/// the 32-hex uid space).
pub fn identity_key(uid: &str, path: &Path) -> String {
    if uid.is_empty() {
        format!("path:{}", path.to_string_lossy())
    } else {
        uid.to_string()
    }
}

/// One plugin's saved state plus the identity needed to decide later whether
/// a loaded instance should receive it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginStateRecord {
    /// VST3 class/component uid; "" when captured before the plugin's uid was
    /// known (filename-only scan). On apply, a loaded plugin whose real uid
    /// disagrees with a non-empty value here is skipped as incompatible.
    pub uid: String,
    /// Bundle path the state was captured from (display + fallback identity).
    pub path: String,
    pub vendor: String,
    pub name: String,
    pub version: String,
    /// Unix epoch milliseconds when the blob was captured.
    pub saved_unix_ms: u64,
    /// Opaque component+controller blob from `Plugin::save_state`.
    pub state: Vec<u8>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct RecordMeta {
    uid: String,
    path: String,
    vendor: String,
    name: String,
    version: String,
    saved_unix_ms: u64,
}

/// The records belonging to one song. Loading never fails fatally: a missing
/// file means "no saved state", and a corrupt/foreign/truncated file is
/// discarded whole rather than partially applied — it must never damage the
/// MIDI or JSON sidecar it sits beside.
#[derive(Debug, Default)]
pub struct PluginStateStore {
    records: BTreeMap<String, PluginStateRecord>,
}

impl PluginStateStore {
    /// Read `path`; any problem yields an empty store.
    pub fn load(path: &Path) -> Self {
        let mut store = Self::default();
        let Ok(bytes) = std::fs::read(path) else {
            return store;
        };
        match decode(&bytes) {
            Ok(records) => {
                store.records = records;
            }
            Err(e) => {
                tracing::warn!("ignoring plugin state file {}: {e}", path.display());
            }
        }
        store
    }

    /// Write the store atomically (temp file + rename). No file is created
    /// for an empty store — a song that never hosted a plugin leaves nothing
    /// behind. Returns whether a write happened.
    pub fn save(&self, path: &Path) -> std::io::Result<bool> {
        if self.records.is_empty() {
            return Ok(false);
        }
        let bytes = encode(&self.records);
        let tmp = path.with_extension("editor.state.tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&bytes)?;
            f.sync_all().ok();
        }
        std::fs::rename(&tmp, path)?;
        Ok(true)
    }

    /// Find the record for a plugin: by class uid first, then by bundle path
    /// (covers records captured before the uid was known), then a last-resort
    /// scan for a uid-keyed record whose stored path matches — the same bundle
    /// at the same location is the same plugin even when the uid differs or
    /// wasn't reported this time around.
    pub fn lookup(&self, uid: &str, path: &Path) -> Option<&PluginStateRecord> {
        if !uid.is_empty() {
            if let Some(rec) = self.records.get(uid) {
                return Some(rec);
            }
        }
        if let Some(rec) = self.records.get(&identity_key("", path)) {
            return Some(rec);
        }
        let path = path.to_string_lossy();
        self.records.values().find(|r| r.path == path.as_ref())
    }

    /// Store a captured blob. Replaces the record under the same identity and
    /// removes a stale path-keyed twin when the uid is now known. Returns true
    /// when the stored bytes actually changed (cheap "needs write" signal).
    pub fn insert(&mut self, record: PluginStateRecord) -> bool {
        let key = identity_key(&record.uid, Path::new(&record.path));
        if !record.uid.is_empty() {
            let path_key = identity_key("", Path::new(&record.path));
            if path_key != key {
                self.records.remove(&path_key);
            }
        }
        if self
            .records
            .get(&key)
            .map(|old| old.state == record.state)
            .unwrap_or(false)
        {
            // refresh identity metadata without counting it as a state change
            if let Some(old) = self.records.get_mut(&key) {
                old.path.clone_from(&record.path);
                old.vendor.clone_from(&record.vendor);
                old.name.clone_from(&record.name);
                old.version.clone_from(&record.version);
                old.saved_unix_ms = record.saved_unix_ms;
                if old.uid.is_empty() && !record.uid.is_empty() {
                    old.uid.clone_from(&record.uid);
                }
            }
            return false;
        }
        self.records.insert(key, record);
        true
    }

}

/// Current unix time in milliseconds (state capture stamp).
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn encode(records: &BTreeMap<String, PluginStateRecord>) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + records.len() * 256);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&(records.len() as u32).to_le_bytes());
    for (key, rec) in records {
        let meta = serde_json::json!({
            "uid": rec.uid,
            "path": rec.path,
            "vendor": rec.vendor,
            "name": rec.name,
            "version": rec.version,
            "saved_unix_ms": rec.saved_unix_ms,
        })
        .to_string();
        out.extend_from_slice(&(key.len() as u32).to_le_bytes());
        out.extend_from_slice(key.as_bytes());
        out.extend_from_slice(&(meta.len() as u32).to_le_bytes());
        out.extend_from_slice(meta.as_bytes());
        out.extend_from_slice(&(rec.state.len() as u64).to_le_bytes());
        out.extend_from_slice(&rec.state);
    }
    out
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| "length overflow".to_string())?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or_else(|| format!("truncated at offset {}", self.pos))?;
        self.pos = end;
        Ok(slice)
    }

    fn u32(&mut self) -> Result<u32, String> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, String> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn utf8(&mut self, n: usize) -> Result<String, String> {
        String::from_utf8(self.take(n)?.to_vec()).map_err(|_| "invalid utf-8".to_string())
    }
}

fn decode(bytes: &[u8]) -> Result<BTreeMap<String, PluginStateRecord>, String> {
    let mut cur = Cursor { bytes, pos: 0 };
    if cur.take(MAGIC.len())? != MAGIC {
        return Err("bad magic".into());
    }
    let version = cur.u32()?;
    if version != FORMAT_VERSION {
        return Err(format!("unsupported version {version}"));
    }
    let count = cur.u32()? as usize;
    let mut records = BTreeMap::new();
    for _ in 0..count {
        let key_len = cur.u32()? as usize;
        if key_len > MAX_META_LEN {
            return Err("key length out of range".into());
        }
        let key = cur.utf8(key_len)?;
        let meta_len = cur.u32()? as usize;
        if meta_len > MAX_META_LEN {
            return Err("meta length out of range".into());
        }
        let meta: RecordMeta =
            serde_json::from_str(&cur.utf8(meta_len)?).map_err(|e| format!("bad meta: {e}"))?;
        let blob_len = cur.u64()?;
        if blob_len > MAX_BLOB_LEN {
            return Err("state blob length out of range".into());
        }
        let state = cur.take(blob_len as usize)?.to_vec();
        records.insert(
            key,
            PluginStateRecord {
                uid: meta.uid,
                path: meta.path,
                vendor: meta.vendor,
                name: meta.name,
                version: meta.version,
                saved_unix_ms: meta.saved_unix_ms,
                state,
            },
        );
    }
    if cur.pos != bytes.len() {
        return Err("trailing bytes".into());
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(uid: &str, path: &str, blob: &[u8]) -> PluginStateRecord {
        PluginStateRecord {
            uid: uid.into(),
            path: path.into(),
            vendor: "Vendor".into(),
            name: "Plugin".into(),
            version: "1.0".into(),
            saved_unix_ms: 123,
            state: blob.to_vec(),
        }
    }

    fn write(store: &PluginStateStore, path: &Path) {
        assert!(store.save(path).unwrap());
    }

    #[test]
    fn roundtrip_preserves_records_and_blobs() {
        let dir = std::env::temp_dir().join(format!("med-ps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("song.mid.editor.state");
        let mut store = PluginStateStore::default();
        store.insert(rec("AABBCCDD", "C:/VST3/a.vst3", &[1, 2, 3]));
        store.insert(rec("", "C:/VST3/b.vst3", &[9; 1000]));
        write(&store, &file);
        let loaded = PluginStateStore::load(&file);
        assert_eq!(loaded.records.len(), 2);
        assert_eq!(
            loaded.lookup("AABBCCDD", Path::new("ignored")).unwrap().state,
            vec![1, 2, 3]
        );
        assert_eq!(
            loaded
                .lookup("", Path::new("C:/VST3/b.vst3"))
                .unwrap()
                .state
                .len(),
            1000
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_empty_not_error() {
        let store = PluginStateStore::load(Path::new("definitely/not/here.editor.state"));
        assert!(store.records.is_empty());
    }

    #[test]
    fn corrupt_files_are_discarded_whole() {
        let dir = std::env::temp_dir().join(format!("med-ps-c-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in [
            ("empty", vec![]),
            ("short", b"MEDP".to_vec()),
            ("badmagic", b"NOTOURS\x01\x00\x00\x00".to_vec()),
            ("badversion", b"MEDPLGST\x63\x00\x00\x00".to_vec()),
            // valid header, record count lies about the remainder
            ("truncated", b"MEDPLGST\x01\x00\x00\x00\x05\x00\x00\x00".to_vec()),
        ] {
            let file = dir.join(name);
            std::fs::write(&file, bytes).unwrap();
            assert!(
                PluginStateStore::load(&file).records.is_empty(),
                "{name} must not load"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_keyed_record_is_found_then_promoted_to_uid() {
        let mut store = PluginStateStore::default();
        let path = Path::new("C:/VST3/x.vst3");
        // captured before the uid was known
        assert!(store.insert(rec("", "C:/VST3/x.vst3", &[7])));
        // same plugin, now with its class uid: lookup by uid misses, path hits
        assert_eq!(
            store.lookup("AABB", path).unwrap().state,
            vec![7],
            "uid-miss must fall back to the path key"
        );
        // capture with the known uid promotes the record off the path key
        assert!(store.insert(rec("AABB", "C:/VST3/x.vst3", &[8])));
        assert_eq!(store.records.len(), 1, "stale path key must be removed");
        assert_eq!(store.lookup("AABB", path).unwrap().state, vec![8]);
        assert_eq!(store.lookup("", path).unwrap().state, vec![8]);
    }

    #[test]
    fn insert_skips_identical_blob_and_keeps_other_records() {
        let mut store = PluginStateStore::default();
        assert!(store.insert(rec("U1", "C:/VST3/a.vst3", &[1])));
        assert!(store.insert(rec("U2", "C:/VST3/b.vst3", &[2])));
        // identical blob -> no change reported
        assert!(!store.insert(rec("U1", "C:/VST3/a.vst3", &[1])));
        // different blob for a different plugin -> U2 record untouched
        assert!(store.insert(rec("U1", "C:/VST3/a.vst3", &[3])));
        assert_eq!(store.lookup("U2", Path::new("x")).unwrap().state, vec![2]);
    }

    #[test]
    fn empty_store_writes_no_file() {
        let dir = std::env::temp_dir().join(format!("med-ps-e-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("song.mid.editor.state");
        assert!(!PluginStateStore::default().save(&file).unwrap());
        assert!(!file.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
