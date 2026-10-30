//! Fuzzes the Claude Desktop transcript-line path
//! (`hh_core::adapter::fuzzing::fuzz_parse_desktop_line`).
//!
//! Claude Desktop's Code tab embeds a Claude Code CLI, so its transcript is the
//! same JSONL `claude_jsonl.rs` already fuzzes (same `user`/`assistant` records,
//! same content blocks — see `hh-core/tests/fixtures/claude-desktop/NOTES.md`).
//! This target exists because the Desktop adapter runs one extra step over a
//! candidate transcript that the Claude Code adapter does not: the
//! `entrypoint` probe used to tell a Desktop session from a terminal `claude`
//! one. It mirrors the live per-line path: UTF-8 validate -> trim -> JSON
//! parse -> entrypoint probe -> convert to events. Must never panic.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    hh_core::adapter::fuzzing::fuzz_parse_desktop_line(data);
});
