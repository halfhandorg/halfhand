//! Fuzz the redaction engine (docs/redaction-design.md): detection and
//! redaction over arbitrary text must never panic, and redacted output must
//! be a detection fixed point. Drives `hh_core::redact::fuzzing`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    hh_core::redact::fuzzing::fuzz_detect_and_redact(data);
});
