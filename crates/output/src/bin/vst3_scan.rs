//! spike: discover installed VST3 plugins, then load the first one and send it
//! a MIDI note via vst3-host (audio backend intentionally off for Phase 0).

use midi_io::EventSink;

fn main() {
    let found = output::discover_plugins();
    println!("== discovered VST3 bundles ==");
    for p in &found.plugins {
        println!("  {} @ {}", p.name, p.path.display());
    }
    if found.plugins.is_empty() {
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

    if let Some(first) = found.plugins.first() {
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

    // full realtime path needs an audio device (none on a headless VM); do an
    // offline render instead — exercises load->process->WAV with no hardware
    if let Some(first) = found.plugins.first() {
        println!("== PluginOutput::open: {} ==", first.path.display());
        match output::PluginOutput::open(&first.path) {
            Ok(out) => {
                println!("open ok; us_per_sample={:?}", out.us_to_samples());
                let mut sink = out.event_sink();
                sink.send_at(&[0x90, 60, 110], 0);
                std::thread::sleep(std::time::Duration::from_millis(300));
                let lvl = out.level();
                println!("after note_on: output level {lvl:.4}");
                std::thread::sleep(std::time::Duration::from_millis(300));
                sink.panic();
                println!("panic sent; level now {:.4}", out.level());
            }
            Err(e) => eprintln!("PluginOutput::open failed: {e}"),
        }

        println!("== offline render_to_wav ==");
        match vst3_host::simple::load_plugin(&first.path) {
            Ok(mut plugin) => {
                use vst3_host::midi::{MidiChannel, MidiEvent};
                let note = MidiEvent::NoteOn {
                    channel: MidiChannel::Ch1,
                    note: 60,
                    velocity: 110,
                };
                let wav = std::path::Path::new("render_test.wav");
                match vst3_host::simple::render_to_wav(&mut plugin, 1.0, &[note], wav) {
                    Ok(()) => {
                        // peak-check the rendered file: silence => no audio produced
                        let bytes = std::fs::read(wav).unwrap_or_default();
                        let n = bytes.len();
                        let floats: Vec<f32> = bytes[44..]
                            .chunks(4)
                            .filter_map(|c| c.try_into().ok())
                            .map(|c: [u8; 4]| f32::from_le_bytes(c))
                            .collect();
                        let peak = floats.iter().map(|f| f.abs()).fold(0.0f32, f32::max);
                        println!("rendered {n}B wav, {peak:.4} peak amplitude");
                    }
                    Err(e) => eprintln!("render_to_wav failed: {e}"),
                }
            }
            Err(e) => eprintln!("reload for render failed: {e}"),
        }
    }
}
