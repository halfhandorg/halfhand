//! Fuzzes the Gemini CLI JSONL transcript-line parser
//! (`hh_core::adapter::fuzzing::fuzz_parse_gemini_line`), which mirrors the live
//! tailer's per-line path: UTF-8 validate -> trim -> JSON parse -> convert to
//! events. Must never panic on arbitrary bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    hh_core::adapter::fuzzing::fuzz_parse_gemini_line(data);
});
