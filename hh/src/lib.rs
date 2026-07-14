//! Library target for the `halfhand` package.
//!
//! The shipped CLI binary lives in `src/main.rs`. This library exists for one
//! reason: the `hh-dist` release-asset generator (a separate, `publish =
//! false` workspace crate) needs to reuse the single source of truth for the
//! CLI surface — [`cli::Cli`] — to generate shell completion scripts and the
//! `hh.1` man page, instead of duplicating the clap definition and letting it
//! drift.
//!
//! Keeping the generator in its own crate (rather than a second `[[bin]]` in
//! this package) preserves `cargo install halfhand` installing exactly one
//! binary (`hh`) — a 1.0 backward-compatibility invariant.
//!
//! `cli` is the only module re-exported; the binary's other modules (replay,
//! export, …) stay private to `main.rs`.
//!
//! # Version
//! `cli::Cli` derives `#[command(version = HH_VERSION)]`. In the binary that
//! const is the git-sha-embedded string from `build.rs` (NFR-8). Here — used
//! only to generate static docs/completions — the plain package version is
//! correct and sufficient, so we define `HH_VERSION` from `CARGO_PKG_VERSION`.

pub mod cli;

/// Package version, without the git sha. Used only when compiling this lib
/// for asset generation; the real `hh` binary embeds the sha via `build.rs`.
pub const HH_VERSION: &str = env!("CARGO_PKG_VERSION");
