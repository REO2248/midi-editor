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
        chunks = &body[data.len()..];
        if len % 2 == 1 && !chunks.is_empty() {
            chunks = &chunks[1..];
        }
    }
    None
}

/// Model midly 0.5.3's `Chunk::read` scan over the SMF payload: skip unknown
/// chunks (BE length, remainder fallback since `strict` is off), and on the
/// first `MThd` chunk return the high byte of the timing division field.
fn division_hi_byte(raw: &[u8]) -> Option<u8> {
    let payload = match raw.get(..4) {
        Some(b"RIFF") => riff_unwrap(raw)?,
        Some(b"MThd") => raw,
        _ => return None,
    };
    let mut rest = payload;
    loop {
        if rest.len() < 8 {
            return None;
        }
        let id = &rest[..4];
        let len = u32::from_be_bytes(rest[4..8].try_into().ok()?) as usize;
        let body = &rest[8..];
        let chunk = &body[..len.min(body.len())];
        if id == b"MThd" {
            // format u16, ntrks u16, then timing; division hi byte is fps.
            return chunk.get(4).copied();
        }
        if id == b"MTrk" || body.len() < len {
            // MTrk isn't a header; a remainder-length chunk consumes all.
            return None;
        }
        rest = &body[len..];
    }
}

/// Known upstream panic, unfixable downstream: midly 0.5.3 negates the SMPTE
/// fps byte as i8, so a division high byte of 0x80 (-128) overflows and
/// panics (`midly/src/primitive.rs` `Timing::read`).
///
/// In normal builds `smf_core::parse` recovers via `catch_unwind` + the
/// lenient path, but libFuzzer builds abort on panic, so targets that run
/// any midly-backed code must skip inputs hitting only this bug to keep
/// finding NEW panics.
pub fn hits_known_midly_panic(raw: &[u8]) -> bool {
    division_hi_byte(raw) == Some(0x80)
}
