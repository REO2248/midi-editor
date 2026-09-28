// Passive monitor: print every MIDI message arriving on the loopMIDI port.
use std::time::{Duration, Instant};

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);
    let ins = midi_io::list_inputs().unwrap();
    let li = ins
        .iter()
        .position(|p| p.name.contains("loopMIDI"))
        .expect("loopMIDI input not found");
    let mut inp = midir::MidiInput::new("midi_listen").unwrap();
    inp.ignore(midir::Ignore::None);
    let port = &inp.ports()[li];
    let _conn = inp
        .connect(
            port,
            "listen",
            move |_ts, msg, _| println!("msg {:02x?}", msg),
            (),
        )
        .unwrap();
    println!("listening {}s on {}", secs, ins[li].name);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(200));
    }
}
