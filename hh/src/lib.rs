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
//! `cli` is the only supported module; `html_terminal` is also public but
//! `#[doc(hidden)]`, solely so the fuzz harness can reach its parser of
//! untrusted terminal output (the binary uses it from here). The binary's
//! other modules (replay, export, …) stay private to `main.rs`.
//!
//! # Version
//! `cli::Cli` derives `#[command(version = HH_VERSION)]`. In the binary that
//! const is the git-sha-embedded string from `build.rs`. Here — used
//! only to generate static docs/completions — the plain package version is
//! correct and sufficient, so we define `HH_VERSION` from `CARGO_PKG_VERSION`.

pub mod cli;

/// Terminal-output → HTML-transcript pipeline used by `hh export --html` /
/// `hh replay --web`. Public only so the `html_terminal` fuzz target can
/// reach the ANSI parser; not a supported API.
#[doc(hidden)]
pub mod html_terminal;

/// Package version, without the git sha. Used only when compiling this lib
/// for asset generation; the real `hh` binary embeds the sha via `build.rs`.
pub const HH_VERSION: &str = env!("CARGO_PKG_VERSION");
