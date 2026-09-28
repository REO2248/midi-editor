use cpal::traits::{DeviceTrait, HostTrait};

fn name_of(d: &cpal::Device) -> String {
    d.description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unnamed>".into())
}

fn main() {
    for h in cpal::available_hosts() {
        let host = match cpal::host_from_id(h) {
            Ok(h) => h,
            Err(e) => {
                println!("host {h:?}: init failed {e}");
                continue;
            }
        };
        println!("host {:?}:", h);
        match host.default_output_device() {
            Some(d) => println!("  default out: {}", name_of(&d)),
            None => println!("  default out: none"),
        }
        match host.output_devices() {
            Ok(ds) => {
                for d in ds {
                    println!("    dev: {}", name_of(&d));
                }
            }
            Err(e) => println!("    enum err: {e}"),
        }
    }
}
