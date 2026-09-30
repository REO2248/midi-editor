//! Emits BUILD_IDENTITY = "<semver>+<short-sha>[.dirty]" so Help->About,
//! `--version`, and MCP serverInfo all report the same build. Deliberately
//! carries no timestamp: identical source must produce an identical string
//! (reproducible build metadata). Mirrors crates/mcp-server/build.rs.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    // untracked files excluded: build artifacts / scratch files must not
    // poison the dirty flag — only tracked content differing from HEAD counts
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    println!(
        "cargo:rustc-env=BUILD_IDENTITY={version}+{sha}{}",
        if dirty { ".dirty" } else { "" }
    );
}
