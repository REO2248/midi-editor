//! Emits BUILD_IDENTITY = "<semver>+<short-sha>[.dirty]" so Help->About,
//! `--version`, and MCP serverInfo all report the same build. Deliberately
//! carries no timestamp: identical source must produce an identical string
//! (reproducible build metadata). `MIDI_EDITOR_COMMIT` in the build
//! environment wins (CI can inject it without git); otherwise ask git;
//! otherwise "unknown" — a build from a tarball must still work.
//! Mirrors crates/mcp-server/build.rs.

use std::process::Command;

fn main() {
    // gpui renders views inside the layout pass, so a deep element tree is
    // constructed on top of the full request_layout chain. Menu construction
    // peaks just over MSVC's default 1 MiB main-thread stack in debug builds;
    // give the binary headroom (link arg is per-binary, tests are unaffected).
    #[cfg(target_env = "msvc")]
    println!("cargo:rustc-link-arg-bins=/STACK:8388608");

    println!("cargo:rerun-if-env-changed=MIDI_EDITOR_COMMIT");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let git = |args: &[&str]| -> Option<String> {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let sha = std::env::var("MIDI_EDITOR_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    // untracked files excluded: build artifacts / scratch files must not
    // poison the dirty flag — only tracked content differing from HEAD counts
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    println!("cargo:rustc-env=MIDI_EDITOR_COMMIT={sha}");
    println!(
        "cargo:rustc-env=BUILD_IDENTITY={version}+{sha}{}",
        if dirty { ".dirty" } else { "" }
    );
}
