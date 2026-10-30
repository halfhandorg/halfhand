//! Fuzzes the Claude Desktop session index-record parser
//! (`hh_core::adapter::fuzzing::fuzz_parse_desktop_index`), which reads the
//! `local_<uuid>.json` files Desktop writes under its config dir
//! (`claude-code-sessions/<org>/<account>/`) to find a session's transcript.
//! External input: must never panic, and must reject a `cliSessionId` that
//! could escape the projects directory when used as a file name.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    hh_core::adapter::fuzzing::fuzz_parse_desktop_index(data);
});
