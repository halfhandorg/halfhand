//! Output rendering helpers for `hh`.
//!
//! Color plumbing for successful output (list/replay) lives with the
//! subcommands that produce it. This module holds the shared "not implemented"
//! error used by every subcommand in this skeleton.

use anyhow::anyhow;

/// Build a structured, actionable "not implemented" error (NFR-7: what failed,
/// why, and a suggested next step).
pub fn not_implemented(subcommand: &str, hint: &str) -> anyhow::Error {
    anyhow!(
        "`hh {subcommand}` is not implemented in this skeleton\n  \
         why: only the CLI surface, config, storage, and blob store are wired up\n  \
         hint: {hint}"
    )
}
