//! Fuzzes `config.toml` parsing (`hh_core::config::fuzzing::fuzz_parse`),
//! covering `parse_bytes`' byte-size suffix parsing and `merge_table`'s value
//! coercion. Must never panic on arbitrary text (SRS 4.2: unknown keys warn,
//! never fail; malformed values must error, not crash).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    hh_core::config::fuzzing::fuzz_parse(data);
});
