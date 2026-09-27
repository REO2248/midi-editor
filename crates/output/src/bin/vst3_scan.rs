//! spike: discover installed VST3 plugins, then load the first one and send it
//! a MIDI note via vst3-host (audio backend intentionally off for Phase 0).

fn main() {
    let found = output::discover_plugins();
    println!("== discovered VST3 bundles ==");
    for p in &found {
        println!("  {} @ {}", p.name, p.path.display());
    }
    if found.is_empty() {
        println!("  (none installed)");
    }

    // also run vst3-host's own discovery for comparison
    match vst3_host::simple::discover_plugins() {
        Ok(infos) => {
            println!("== vst3-host discover_plugins ==");
            for i in &infos {
                println!(
                    "  {} v{} | {} audio out | midi in: {} | gui: {}",
                    i.name, i.version, i.audio_outputs, i.has_midi_input, i.has_gui
                );
            }
            if infos.is_empty() {
                println!("  (none)");
            }
        }
        Err(e) => eprintln!("vst3-host discovery error: {e}"),
    }

    if let Some(first) = found.first() {
        println!("== attempting load: {} ==", first.path.display());
        match vst3_host::simple::load_plugin(&first.path) {
            Ok(mut plugin) => {
                println!("loaded ok; sending note on");
                use vst3_host::midi::MidiChannel;
                match plugin.start_processing() {
                    Ok(()) => println!("start_processing ok"),
                    Err(e) => eprintln!("start_processing: {e}"),
                }
                match plugin.send_midi_note(60, 100, MidiChannel::Ch1) {
                    Ok(()) => println!("send_midi_note ok"),
                    Err(e) => eprintln!("send_midi_note: {e}"),
                }
            }
            Err(e) => eprintln!("load failed: {e}"),
        }
    }
}
