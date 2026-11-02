//! Fuzz the HTML export's terminal-output pipeline (`hh export --html`,
//! `hh replay --web`): arbitrary recorded terminal text goes through the
//! ANSI parser, cross-segment redaction, and truncation. It must never panic,
//! must never emit a control byte other than `\n`/`\t`, and must never emit a
//! CSS class outside the `hh-t-*` vocabulary. Drives
//! `halfhand::html_terminal::fuzz_pipeline`.

#![no_main]

use libfuzzer_sys::fuzz_target;
use std::sync::OnceLock;

fn detectors() -> &'static hh_core::Detectors {
    static D: OnceLock<hh_core::Detectors> = OnceLock::new();
    D.get_or_init(|| {
        hh_core::Detectors::new(&hh_core::RedactionConfig::default())
            .expect("default detectors compile")
    })
}

fuzz_target!(|input: (u16, &str)| {
    let (max, raw) = input;
    halfhand::html_terminal::fuzz_pipeline(raw, detectors(), usize::from(max));
});
