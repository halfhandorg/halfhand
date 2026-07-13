//! Fuzzes `hh_core::bundle::parse` (`hh import file.hh`'s untrusted-input
//! entry point): arbitrary bytes fed through zstd decode, tar unpack,
//! manifest/events JSON parsing, and blob hash verification must never
//! panic — only ever return `Ok`/`Err`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    hh_core::bundle::fuzzing::fuzz_parse(bytes);
});
