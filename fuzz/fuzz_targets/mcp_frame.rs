//! Fuzzes the MCP stdio proxy's NDJSON line classifier
//! (`hh_record::fuzzing::fuzz_upstream_line` / `fuzz_downstream_line`), which
//! is exactly the code path the live proxy runs on every line from either
//! side of the wire. Must never panic on arbitrary bytes.

#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    upstream: bool,
    line: &'a [u8],
}

fuzz_target!(|input: Input| {
    if input.upstream {
        hh_record::fuzzing::fuzz_upstream_line(input.line);
    } else {
        hh_record::fuzzing::fuzz_downstream_line(input.line);
    }
});
