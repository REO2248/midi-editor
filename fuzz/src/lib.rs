//! Shared helpers for the fuzz targets.

/// Model midly 0.5.3's `riff::unwrap`: RIFF chunk (LE length, body clamped to
/// remaining bytes), RMID formtype, then scan sub-chunks for `data`.
fn riff_unwrap(raw: &[u8]) -> Option<&[u8]> {
    if raw.len() < 8 {
        return None;
    }
    let len = u32::from_le_bytes(raw[4..8].try_into().ok()?) as usize;
    let avail = &raw[8..];
    let riff = &avail[..len.min(avail.len())];
    if riff.get(..4) != Some(b"RMID") {
        return None;
    }
    let mut chunks = &riff[4..];
    while chunks.len() >= 8 {
        let id = &chunks[..4];
        let len = u32::from_le_bytes(chunks[4..8].try_into().ok()?) as usize;
        let body = &chunks[8..];
        let data = &body[..len.min(body.len())];
        if id == b"data" {
            return Some(data);
        }
        if body.len() < len {
            // remainder of the file becomes the chunk body -> EOF after it
            return None;
        }
        chunks = &body[len..];
        if len % 2 == 1 && !chunks.is_empty() {
            chunks = &chunks[1..];
        }
    }
    None
}

/// Model midly 0.5.3's `Chunk::read` walk over the whole SMF payload: chunks
/// are skipped by big-endian length (remainder fallback since `strict` is
/// off), and `Header::read` runs on EVERY `MThd` chunk reached — including
/// stray ones mid-file while `TrackIter` scans for tracks. Returns true when
/// any reached MThd carries division high byte 0x80 (the panicking value).
fn any_header_panics(payload: &[u8]) -> bool {
    let mut rest = payload;
    while rest.len() >= 8 {
        let id = &rest[..4];
        let len = u32::from_be_bytes(rest[4..8].try_into().unwrap()) as usize;
        let body = &rest[8..];
        let chunk = &body[..len.min(body.len())];
        if id == b"MThd" && chunk.get(4) == Some(&0x80) {
            return true;
        }
        if body.len() < len {
            // remainder consumed by this chunk -> EOF next
            return false;
        }
        rest = &body[len..];
    }
    false
}

/// Known upstream panic, unfixable downstream: midly 0.5.3 negates the SMPTE
/// fps byte as i8, so a division high byte of 0x80 (-128) overflows and
/// panics (`midly/src/primitive.rs` `Timing::read`). It fires on every MThd
/// chunk the chunk iterator reaches, not just the file's real header.
///
/// In normal builds `smf_core::parse` recovers via `catch_unwind` + the
/// lenient path, but libFuzzer builds abort on panic, so targets that run
/// any midly-backed code must skip inputs hitting only this bug to keep
/// finding NEW panics.
pub fn hits_known_midly_panic(raw: &[u8]) -> bool {
    let payload = match raw.get(..4) {
        Some(b"RIFF") => match riff_unwrap(raw) {
            Some(p) => p,
            None => return false,
        },
        Some(b"MThd") => raw,
        // midly errors before parsing anything else
        _ => return false,
    };
    any_header_panics(payload)
}
