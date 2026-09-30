//! Build id plumbing for `editor_info`: the MCP surface reports the source
//! commit so agents can version-detect a running editor. `MIDI_EDITOR_COMMIT`
//! in the build environment wins (CI can inject it without git); otherwise
//! ask git; otherwise "unknown" — a build from a tarball must still work.

fn main() {
    println!("cargo:rerun-if-env-changed=MIDI_EDITOR_COMMIT");
    let commit = std::env::var("MIDI_EDITOR_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::process::Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=MIDI_EDITOR_COMMIT={commit}");
}
