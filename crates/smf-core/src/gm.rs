//! General MIDI / GS / XG display-name tables and reset-SysEx detection.
//!
//! These names are for presentation only — they are never consulted when
//! serializing, so a friendly name can never rewrite file bytes. The table
//! lives here (not in the app) so both the GUI and MCP surfaces share one
//! map, and a future custom instrument-definition map can be substituted
//! without touching SMF serialization.

/// Which instrument family's conventions a file appears to target. Detected
/// from the well-known "mode reset" SysEx messages; purely a display hint —
/// files without one still get GM names on bank 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeHint {
    /// GM System On: 7E 7F 09 01
    Gm1,
    /// GM2 System On: 7E 7F 09 03
    Gm2,
    /// Roland GS Reset: 41 <dev> 42 12 40 00 7F 00 <chk>
    Gs,
    /// Yamaha XG System On: 43 <dev> 4C 00 00 7E 00
    Xg,
}

impl ModeHint {
    pub fn label(&self) -> &'static str {
        match self {
            ModeHint::Gm1 => "GM",
            ModeHint::Gm2 => "GM2",
            ModeHint::Gs => "GS",
            ModeHint::Xg => "XG",
        }
    }
}

/// Inspect a SysEx payload (as stored — leading F0 already stripped) for a
/// known reset message. Non-destructive: returns a hint, edits nothing.
pub fn reset_hint(payload: &[u8]) -> Option<ModeHint> {
    // trim a trailing F7 terminator for prefix matching
    let p = payload.strip_suffix(&[0xF7]).unwrap_or(payload);
    match p {
        [0x7E, 0x7F, 0x09, 0x01] => Some(ModeHint::Gm1),
        [0x7E, 0x7F, 0x09, 0x03] => Some(ModeHint::Gm2),
        // GS reset: manufacturer 41, any device id, model 42, then the
        // address 40 00 7F and value 00 (checksum byte may follow)
        [0x41, _, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, ..] => Some(ModeHint::Gs),
        // XG system on: manufacturer 43, any device id, model 4C, then the
        // fixed parameter address 00 00 7E and value 00
        [0x43, _, 0x4C, 0x00, 0x00, 0x7E, 0x00, ..] => Some(ModeHint::Xg),
        _ => None,
    }
}

/// GM Level 1 melodic program names (programs 0-127). Shared by GM1, GM2,
/// GS and XG for bank (0,0); other banks stay numeric.
pub const GM_PROGRAM_NAMES: [&str; 128] = [
    "Acoustic Grand Piano",
    "Bright Acoustic Piano",
    "Electric Grand Piano",
    "Honky-tonk Piano",
    "Electric Piano 1",
    "Electric Piano 2",
    "Harpsichord",
    "Clavinet",
    "Celesta",
    "Glockenspiel",
    "Music Box",
    "Vibraphone",
    "Marimba",
    "Xylophone",
    "Tubular Bells",
    "Dulcimer",
    "Drawbar Organ",
    "Percussive Organ",
    "Rock Organ",
    "Church Organ",
    "Reed Organ",
    "Accordion",
    "Harmonica",
    "Tango Accordion",
    "Acoustic Guitar (nylon)",
    "Acoustic Guitar (steel)",
    "Electric Guitar (jazz)",
    "Electric Guitar (clean)",
    "Electric Guitar (muted)",
    "Overdriven Guitar",
    "Distortion Guitar",
    "Guitar Harmonics",
    "Acoustic Bass",
    "Electric Bass (finger)",
    "Electric Bass (pick)",
    "Fretless Bass",
    "Slap Bass 1",
    "Slap Bass 2",
    "Synth Bass 1",
    "Synth Bass 2",
    "Violin",
    "Viola",
    "Cello",
    "Contrabass",
    "Tremolo Strings",
    "Pizzicato Strings",
    "Orchestral Harp",
    "Timpani",
    "String Ensemble 1",
    "String Ensemble 2",
    "Synth Strings 1",
    "Synth Strings 2",
    "Choir Aahs",
    "Voice Oohs",
    "Synth Voice",
    "Orchestra Hit",
    "Trumpet",
    "Trombone",
    "Tuba",
    "Muted Trumpet",
    "French Horn",
    "Brass Section",
    "Synth Brass 1",
    "Synth Brass 2",
    "Soprano Sax",
    "Alto Sax",
    "Tenor Sax",
    "Baritone Sax",
    "Oboe",
    "English Horn",
    "Bassoon",
    "Clarinet",
    "Piccolo",
    "Flute",
    "Recorder",
    "Pan Flute",
    "Blown Bottle",
    "Shakuhachi",
    "Whistle",
    "Ocarina",
    "Lead 1 (square)",
    "Lead 2 (sawtooth)",
    "Lead 3 (calliope)",
    "Lead 4 (chiff)",
    "Lead 5 (charang)",
    "Lead 6 (voice)",
    "Lead 7 (fifths)",
    "Lead 8 (bass + lead)",
    "Pad 1 (new age)",
    "Pad 2 (warm)",
    "Pad 3 (polysynth)",
    "Pad 4 (choir)",
    "Pad 5 (bowed)",
    "Pad 6 (metallic)",
    "Pad 7 (halo)",
    "Pad 8 (sweep)",
    "FX 1 (rain)",
    "FX 2 (soundtrack)",
    "FX 3 (crystal)",
    "FX 4 (atmosphere)",
    "FX 5 (brightness)",
    "FX 6 (goblins)",
    "FX 7 (echoes)",
    "FX 8 (sci-fi)",
    "Sitar",
    "Banjo",
    "Shamisen",
    "Koto",
    "Kalimba",
    "Bagpipe",
    "Fiddle",
    "Shanai",
    "Tinkle Bell",
    "Agogo",
    "Steel Drums",
    "Woodblock",
    "Taiko Drum",
    "Melodic Tom",
    "Synth Drum",
    "Reverse Cymbal",
    "Guitar Fret Noise",
    "Breath Noise",
    "Seashore",
    "Bird Tweet",
    "Telephone Ring",
    "Helicopter",
    "Applause",
    "Gunshot",
];

/// GM program name — always defined (the table covers all 128).
pub fn gm_program_name(program: u8) -> &'static str {
    GM_PROGRAM_NAMES[(program & 0x7F) as usize]
}

/// Standard GM percussion kit names for notes 35-81 (channel 10).
/// Outside that range a note is not a standard GM drum sound.
pub fn gm_drum_name(note: u8) -> Option<&'static str> {
    const NAMES: [&str; 47] = [
        "Acoustic Bass Drum",
        "Bass Drum 1",
        "Side Stick",
        "Acoustic Snare",
        "Hand Clap",
        "Electric Snare",
        "Low Floor Tom",
        "Closed Hi-Hat",
        "High Floor Tom",
        "Pedal Hi-Hat",
        "Low Tom",
        "Open Hi-Hat",
        "Low-Mid Tom",
        "Hi-Mid Tom",
        "Crash Cymbal 1",
        "High Tom",
        "Ride Cymbal 1",
        "Chinese Cymbal",
        "Ride Bell",
        "Tambourine",
        "Splash Cymbal",
        "Cowbell",
        "Crash Cymbal 2",
        "Vibraslap",
        "Ride Cymbal 2",
        "Hi Bongo",
        "Low Bongo",
        "Mute Hi Conga",
        "Open Hi Conga",
        "Low Conga",
        "High Timbale",
        "Low Timbale",
        "High Agogo",
        "Low Agogo",
        "Cabasa",
        "Maracas",
        "Short Whistle",
        "Long Whistle",
        "Short Guiro",
        "Long Guiro",
        "Claves",
        "Hi Wood Block",
        "Low Wood Block",
        "Mute Cuica",
        "Open Cuica",
        "Mute Triangle",
        "Open Triangle",
    ];
    if (35..=81).contains(&note) {
        Some(NAMES[(note - 35) as usize])
    } else {
        None
    }
}

/// Drum-kit label for the percussion channel's bank when the file's detected
/// mode says which convention applies. Only the identities that are
/// unambiguous are named — anything else stays numeric.
pub fn kit_name(mode: ModeHint, bank_msb: u8) -> Option<&'static str> {
    match mode {
        // XG drums: bank MSB 127 (drum kits) / 126 (SFX kits)
        ModeHint::Xg if bank_msb == 127 => Some("XG Drums"),
        ModeHint::Xg if bank_msb == 126 => Some("XG SFX"),
        // GS drums: bank MSB 128 (any LSB) selects the drum bank
        ModeHint::Gs if bank_msb == 128 => Some("GS Drums"),
        // GM melodic file: channel 10 is still the percussion channel
        _ if bank_msb == 0 => Some("Standard Kit"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reset_hints_match_known_signatures() {
        assert_eq!(
            reset_hint(&[0x7E, 0x7F, 0x09, 0x01, 0xF7]),
            Some(ModeHint::Gm1)
        );
        assert_eq!(reset_hint(&[0x7E, 0x7F, 0x09, 0x03]), Some(ModeHint::Gm2));
        assert_eq!(
            reset_hint(&[0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0x7F, 0x00, 0x41, 0xF7]),
            Some(ModeHint::Gs)
        );
        assert_eq!(
            reset_hint(&[0x43, 0x10, 0x4C, 0x00, 0x00, 0x7E, 0x00, 0xF7]),
            Some(ModeHint::Xg)
        );
        // lookalikes and non-resets are not hints
        assert_eq!(reset_hint(&[0x7E, 0x7F, 0x09, 0x02]), None);
        assert_eq!(reset_hint(&[0x43, 0x10, 0x4C, 0x00, 0x00, 0x04]), None);
    }

    #[test]
    fn names_cover_full_ranges() {
        assert_eq!(gm_program_name(0), "Acoustic Grand Piano");
        assert_eq!(gm_program_name(127), "Gunshot");
        assert_eq!(gm_drum_name(36), Some("Bass Drum 1"));
        assert_eq!(gm_drum_name(38), Some("Acoustic Snare"));
        assert_eq!(gm_drum_name(34), None);
        assert_eq!(gm_drum_name(82), None);
        assert_eq!(kit_name(ModeHint::Xg, 127), Some("XG Drums"));
        assert_eq!(kit_name(ModeHint::Gs, 128), Some("GS Drums"));
        assert_eq!(kit_name(ModeHint::Gs, 12), None);
    }
}
