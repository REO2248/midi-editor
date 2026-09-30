use midi_io::EventSink;
use std::time::Duration;

fn main() {
    let path = std::path::Path::new(r"C:\Program Files\Common Files\VST3\Dexed.vst3");
    println!("opening plugin output (cpal + live audio)...");
    let out = match output::PluginOutput::open(path) {
        Ok(p) => p,
        Err(e) => {
            println!("open failed: {e}");
            return;
        }
    };
    println!("opened. us_to_samples={}", out.us_to_samples());

    let mut sink = out.event_sink();
    sink.send_at(&[0x90, 60, 120], 0); // noteOn C4
    sink.send_at(&[0x90, 67, 120], 0); // noteOn G4

    let t0 = std::time::Instant::now();
    let mut peak = 0.0f32;
    while t0.elapsed() < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(100));
        let l = out.level();
        if l > peak {
            peak = l;
        }
    }
    sink.send_at(&[0x80, 60, 0], 0);
    sink.send_at(&[0x80, 67, 0], 0);
    println!(
        "RESULT: peak={peak:.4} {}",
        if peak > 0.001 { "AUDIO OK" } else { "silent" }
    );
}
