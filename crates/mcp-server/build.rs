//! Build id plumbing for `editor_info` + `serverInfo`: the MCP surface
//! reports BUILD_IDENTITY = "<semver>+<short-sha>[.dirty]" — the same string
//! the app shows in Help->About and `--version`. `MIDI_EDITOR_COMMIT` in the
//! build environment wins (CI can inject it without git); otherwise ask git;
//! otherwise "unknown" — a build from a tarball must still work. No
//! timestamp: identical source produces an identical string. Mirrors
//! crates/app/build.rs.

fn main() {
    println!("cargo:rerun-if-env-changed=MIDI_EDITOR_COMMIT");
    // Re-resolve when the commit may have moved: HEAD itself (branch
    // switches / detached HEAD) and the ref it points at (new commits —
    // falling back to packed-refs when the loose ref doesn't exist).
    let git = |args: &[&str]| -> Option<String> {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if let Some(gitdir) = git(&["rev-parse", "--absolute-git-dir"]) {
        let gitdir = std::path::PathBuf::from(gitdir);
        println!("cargo:rerun-if-changed={}", gitdir.join("HEAD").display());
        if let Some(head) = git(&["symbolic-ref", "-q", "HEAD"]) {
            let loose = gitdir.join(&head);
            let watch = if loose.exists() {
                loose
            } else {
                gitdir.join("packed-refs")
            };
            if watch.exists() {
                println!("cargo:rerun-if-changed={}", watch.display());
            }
        }
    }
    let commit = std::env::var("MIDI_EDITOR_COMMIT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| git(&["rev-parse", "--short", "HEAD"]))
        .unwrap_or_else(|| "unknown".into());
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    println!("cargo:rustc-env=MIDI_EDITOR_COMMIT={commit}");
    println!(
        "cargo:rustc-env=BUILD_IDENTITY={version}+{commit}{}",
        if dirty { ".dirty" } else { "" }
    );
}
