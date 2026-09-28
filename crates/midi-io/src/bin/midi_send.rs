// Record-path test sender: play a short phrase into "loopMIDI Port" so a
// listening midi_io::Input (the app's rec mode) sees real timestamps.
use std::time::Duration;

fn main() {
    let outs = midi_io::list_outputs().unwrap();
    let lo = outs
        .iter()
        .position(|p| p.name.contains("loopMIDI"))
        .expect("loopMIDI output");
    let mut out = midi_io::Output::open(lo).unwrap();
    for (i, key) in [60u8, 64, 67].into_iter().enumerate() {
        out.send(&[0x90, key, 100]).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        out.send(&[0x80, key, 0]).unwrap();
        if i < 2 {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    println!("sent 3 notes");
}
