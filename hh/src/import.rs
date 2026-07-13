//! `hh import` — import a portable session bundle produced by
//! `hh export --bundle` (`hh_core::bundle`).
//!
//! Validates the bundle's manifest, `format_version`, and blob hashes, then
//! imports it under a brand-new local session id (the original id is
//! preserved in `sessions.imported_from`). Every failure mode reports
//! precisely what is wrong — not one generic "corrupt bundle" message — per
//! CLAUDE.md's actionable-errors rule.

use crate::cli;
use crate::render;
use owo_colors::OwoColorize;
use std::process::ExitCode;

/// `hh import <file.hh>`.
pub(crate) fn import_command(args: &cli::ImportArgs) -> anyhow::Result<ExitCode> {
    let (store, _paths, _config) = crate::open_store()?;

    let bytes = std::fs::read(&args.file).map_err(|e| {
        anyhow::anyhow!(
            "could not read bundle {}\n  why: {e}\n  hint: check the path and that the file exists",
            args.file.display()
        )
    })?;
    let bundle = hh_core::bundle::parse(&bytes)
        .map_err(|e| anyhow::anyhow!("could not import {}\n  why: {e}", args.file.display()))?;

    let created = store
        .import(&bundle)
        .map_err(|e| anyhow::anyhow!("could not import bundle into the local store\n  why: {e}"))?;
    let session = store.get_session(&created.id).map_err(|e| {
        anyhow::anyhow!("import succeeded but could not reload the session\n  why: {e}")
    })?;

    let color = render::use_color();
    let check = if color {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };
    let original = bundle
        .session
        .get("short_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| bundle.session.get("id").and_then(serde_json::Value::as_str))
        .unwrap_or("unknown");
    println!(
        "{check} Imported session {sid} (from {original}) · {steps} steps · hh replay {sid}",
        sid = session.short_id,
        steps = session.step_count,
    );
    Ok(ExitCode::SUCCESS)
}
