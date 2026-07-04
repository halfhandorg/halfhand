//! Build script: embed the git sha into `HH_VERSION` (NFR-8).
//!
//! Writes a `pub const HH_VERSION: &str` to `$OUT_DIR/hh_version.rs`, which
//! `main.rs` includes. No network, no `unwrap()`/`expect()` (workspace clippy
//! runs against build scripts too).

use std::io::Write;
use std::path::Path;
use std::process::Command;

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn main() {
    let sha =
        git_output(&["rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".to_string());
    let dirty = git_output(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    let pkg = env!("CARGO_PKG_VERSION");
    let suffix = if dirty { " (dirty)" } else { "" };
    let line = format!("pub const HH_VERSION: &str = \"{pkg} ({sha}){suffix}\";\n");

    // OUT_DIR is always set by Cargo for build scripts; if absent, skip.
    let Ok(out_dir) = std::env::var("OUT_DIR") else {
        return;
    };
    let path = Path::new(&out_dir).join("hh_version.rs");
    if let Ok(mut f) = std::fs::File::create(&path) {
        let _ = f.write_all(line.as_bytes());
    }
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}
