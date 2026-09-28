// loopMIDI end-to-end: send via midi_io::Output, receive on midir input
use midi_io::EventSink;
use std::sync::mpsc;
use std::time::Duration;

fn main() {
    let outs = midi_io::list_outputs().unwrap();
    let ins = midi_io::list_inputs().unwrap();
    let li = ins.iter().position(|p| p.name.contains("loopMIDI")).expect("loopMIDI input");
    let lo = outs.iter().position(|p| p.name.contains("loopMIDI")).expect("loopMIDI output");

    // input listener
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let mut inp = midir::MidiInput::new("probe").unwrap();
    inp.ignore(midir::Ignore::None);
    let port = &inp.ports()[li];
    let _conn = inp.connect(port, "probe-in", move |_ts, bytes, _| {
        let _ = tx.send(bytes.to_vec());
    }, ()).unwrap();

    let mut out = midi_io::Output::open(lo).unwrap();
    out.send(&[0x90, 60, 100]).unwrap();
    out.send(&[0xB0, 7, 77]).unwrap();
    let mut got = 0;
    let t0 = std::time::Instant::now();
    while t0.elapsed() < Duration::from_secs(3) && got < 2 {
        if let Ok(b) = rx.recv_timeout(Duration::from_millis(500)) {
            println!("recv {:02X?}", b);
            got += 1;
        }
    }
    println!("RESULT: {got}/2 messages looped back");
}
