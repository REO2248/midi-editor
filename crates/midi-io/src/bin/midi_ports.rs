//! spike: enumerate MIDI in/out ports on this machine.

fn main() {
    println!("== MIDI outputs ==");
    match midi_io::list_outputs() {
        Ok(ports) => {
            for p in &ports {
                println!("  [{}] {}", p.index, p.name);
            }
            if ports.is_empty() {
                println!("  (none)");
            }
        }
        Err(e) => eprintln!("error: {e}"),
    }
    println!("== MIDI inputs ==");
    match midi_io::list_inputs() {
        Ok(ports) => {
            for p in &ports {
                println!("  [{}] {}", p.index, p.name);
            }
            if ports.is_empty() {
                println!("  (none)");
            }
        }
        Err(e) => eprintln!("error: {e}"),
    }
}
