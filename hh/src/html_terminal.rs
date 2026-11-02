//! Terminal output for the HTML replay page (`hh export --html`,
//! `hh replay --web`).
//!
//! Recorded terminal output is **untrusted** — it is whatever bytes the
//! agent (or anything it ran) wrote to the PTY. This module turns it into an
//! inert, styled transcript the page can render without ever interpreting it
//! as markup:
//!
//! 1. [`Transcript::push_chunk`] collects the session's output chunks in
//!    order (input chunks from `--record-input` are skipped — keystrokes are
//!    not "output", and may carry typed passwords).
//! 2. [`Transcript::finish`] parses the joined text with a small ANSI state
//!    machine: SGR color/attribute codes become a whitelisted CSS class per
//!    run of text; every other escape sequence (cursor movement, OSC titles
//!    and hyperlinks, DCS, charset selection, C1 controls) and every C0
//!    control other than `\n`/`\t` is dropped. `\r\n` is a newline; a lone
//!    `\r` rewinds the current line (progress bars) and `\b` erases one char.
//! 3. [`redact_segments`] runs the secret detectors over the **joined plain
//!    text** (so a secret split across chunks or across a color change is
//!    still one match), splicing redaction tokens back into the segments.
//! 4. [`truncate_segments`] — applied *after* redaction, so a cut can never
//!    split a secret into undetectable halves — keeps the head and tail of an
//!    over-budget transcript around a visible marker.
//!
//! The page's JS only ever assigns segment text via `textContent`, and the
//! class string is built here from a fixed vocabulary (and re-validated
//! client-side), so no recorded byte can become markup.
//!
//! Lives in the `halfhand` library target (doc-hidden) rather than only in
//! the binary so the `html_terminal` fuzz target can reach the parser.

use hh_core::Detectors;

/// One styled run of terminal text: `(text, css_class)`. `css_class` is
/// empty for default styling, otherwise space-separated tokens from the fixed
/// `hh-t-*` vocabulary built by [`Style::class`].
pub type Segment = (String, String);

/// Current SGR state while parsing.
// The four bools are independent SGR attributes that combine freely; a
// state enum would need 16 variants.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Style {
    fg: Option<u8>,
    bg: Option<u8>,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
}

impl Style {
    /// The CSS class string for this style — only ever composed from the
    /// literal prefixes below plus a `0..=15` palette index.
    fn class(self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(f) = self.fg {
            parts.push(format!("hh-t-f{f}"));
        }
        if let Some(b) = self.bg {
            parts.push(format!("hh-t-g{b}"));
        }
        if self.bold {
            parts.push("hh-t-b".into());
        }
        if self.dim {
            parts.push("hh-t-d".into());
        }
        if self.italic {
            parts.push("hh-t-i".into());
        }
        if self.underline {
            parts.push("hh-t-u".into());
        }
        parts.join(" ")
    }

    /// Apply one SGR parameter list (`ESC [ … m`).
    fn apply_sgr(&mut self, params: &str) {
        let codes: Vec<u16> = if params.is_empty() {
            vec![0]
        } else {
            params
                .split([';', ':'])
                .map(|p| p.parse::<u16>().unwrap_or(0))
                .collect()
        };
        let mut i = 0;
        while i < codes.len() {
            let c = codes[i];
            match c {
                0 => *self = Self::default(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                30..=37 => self.fg = u8::try_from(c - 30).ok(),
                39 => self.fg = None,
                40..=47 => self.bg = u8::try_from(c - 40).ok(),
                49 => self.bg = None,
                90..=97 => self.fg = u8::try_from(c - 90 + 8).ok(),
                100..=107 => self.bg = u8::try_from(c - 100 + 8).ok(),
                38 | 48 => {
                    // Extended color: `38;5;n` (256-color) or `38;2;r;g;b`
                    // (truecolor). Only the 16 base palette entries map to a
                    // class; anything else falls back to the default color.
                    let (color, consumed) = match codes.get(i + 1) {
                        Some(5) => {
                            let n = codes.get(i + 2).copied();
                            (n.filter(|n| *n < 16).and_then(|n| u8::try_from(n).ok()), 2)
                        }
                        Some(2) => (None, 4),
                        _ => (None, 0),
                    };
                    if c == 38 {
                        self.fg = color;
                    } else {
                        self.bg = color;
                    }
                    i += consumed;
                }
                _ => {}
            }
            i += 1;
        }
    }
}

/// Accumulates a session's terminal output chunks for the HTML page.
#[derive(Debug, Default)]
pub struct Transcript {
    raw: String,
    chunks: usize,
    omitted_binary_bytes: u64,
}

impl Transcript {
    /// Add one `terminal_output` event body. UTF-8 output chunks are
    /// appended; input chunks are skipped; non-UTF-8 chunks (stored as a
    /// blob or base64) are counted and surfaced as a note on the page rather
    /// than decoded into mojibake.
    pub fn push_chunk(&mut self, body: Option<&serde_json::Value>) {
        let Some(body) = body else { return };
        if body.get("direction").and_then(serde_json::Value::as_str) == Some("input") {
            return;
        }
        if let Some(text) = body.get("text").and_then(serde_json::Value::as_str) {
            self.raw.push_str(text);
            self.chunks += 1;
        } else {
            let size = body
                .get("size")
                .and_then(serde_json::Value::as_u64)
                .or_else(|| {
                    body.get("bytes")
                        .and_then(serde_json::Value::as_str)
                        .map(|b| (b.len() as u64) * 3 / 4)
                })
                .unwrap_or(0);
            self.omitted_binary_bytes += size;
            self.chunks += 1;
        }
    }

    /// `None` when the session recorded no terminal output at all.
    pub fn finish(self) -> Option<serde_json::Value> {
        if self.chunks == 0 {
            return None;
        }
        let segments = parse_ansi(&self.raw);
        Some(serde_json::json!({
            "segments": segments_to_json(&segments),
            "bytes": plain_len(&segments),
            "truncated_bytes": 0,
            "omitted_binary_bytes": self.omitted_binary_bytes,
        }))
    }
}

/// Total UTF-8 byte length of the segments' text.
fn plain_len(segments: &[Segment]) -> usize {
    segments.iter().map(|(t, _)| t.len()).sum()
}

fn segments_to_json(segments: &[Segment]) -> serde_json::Value {
    serde_json::Value::Array(
        segments
            .iter()
            .map(|(t, c)| serde_json::json!([t, c]))
            .collect(),
    )
}

/// Read `[[text, class], …]` back out of a payload. Malformed entries are
/// dropped (the payload is built by this module, so this never happens in
/// practice; dropping is the fail-closed choice).
pub fn segments_from_json(v: &serde_json::Value) -> Vec<Segment> {
    v.as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| {
                    let t = s.get(0)?.as_str()?;
                    let c = s.get(1)?.as_str()?;
                    Some((t.to_string(), c.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Append `text` in `class` to `out`, coalescing with the previous run when
/// the style matches.
fn push_text(out: &mut Vec<Segment>, text: &str, class: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last.1 == class {
            last.0.push_str(text);
            return;
        }
    }
    out.push((text.to_string(), class.to_string()));
}

/// Erase back to the start of the current line (lone `\r`).
fn rewind_line(out: &mut Vec<Segment>) {
    while let Some(last) = out.last_mut() {
        if let Some(nl) = last.0.rfind('\n') {
            last.0.truncate(nl + 1);
            return;
        }
        out.pop();
    }
}

/// Erase one character (`\b`), never crossing a newline.
fn backspace(out: &mut Vec<Segment>) {
    while let Some(last) = out.last_mut() {
        match last.0.chars().last() {
            Some('\n') => return,
            Some(_) => {
                last.0.pop();
                if last.0.is_empty() {
                    out.pop();
                }
                return;
            }
            None => {
                out.pop();
            }
        }
    }
}

/// Parse raw terminal text into styled, control-free segments. Never
/// panics on any input (it is fuzzed via the unit property test below and
/// only ever indexes through `char` iteration).
pub fn parse_ansi(raw: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    let mut style = Style::default();
    let mut class = String::new();
    let mut buf = String::new();
    let mut chars = raw.chars().peekable();

    macro_rules! flush {
        () => {
            push_text(&mut out, &buf, &class);
            buf.clear();
        };
    }

    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' | '\u{9b}' => {
                flush!();
                // `\u{9b}` is the 8-bit CSI; treat it as `ESC [`.
                let kind = if c == '\u{9b}' {
                    Some('[')
                } else {
                    chars.next()
                };
                match kind {
                    Some('[') => {
                        let mut params = String::new();
                        let mut fin = None;
                        for p in chars.by_ref() {
                            if ('\u{40}'..='\u{7e}').contains(&p) {
                                fin = Some(p);
                                break;
                            }
                            // Bound pathological parameter strings.
                            if params.len() < 64 {
                                params.push(p);
                            }
                        }
                        if fin == Some('m') {
                            style.apply_sgr(&params);
                            class = style.class();
                        }
                    }
                    // OSC / DCS / SOS / PM / APC: string terminated by BEL
                    // or ST (`ESC \`). Dropped wholesale — this is where
                    // window titles and hyperlink targets live.
                    Some(']' | 'P' | 'X' | '^' | '_') => {
                        while let Some(p) = chars.next() {
                            if p == '\u{7}' || p == '\u{9c}' {
                                break;
                            }
                            if p == '\u{1b}' {
                                if chars.peek() == Some(&'\\') {
                                    chars.next();
                                }
                                break;
                            }
                        }
                    }
                    // Charset designation takes one more byte.
                    Some('(' | ')' | '*' | '+' | '-' | '.' | '/' | '#' | '%' | ' ') => {
                        chars.next();
                    }
                    // Any other two-byte escape (or a trailing ESC).
                    _ => {}
                }
            }
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    continue; // the `\n` arm handles it
                }
                flush!();
                rewind_line(&mut out);
            }
            '\u{8}' => {
                flush!();
                backspace(&mut out);
            }
            '\n' | '\t' => buf.push(c),
            c if c.is_control() => {} // other C0/C1 controls, DEL
            c => buf.push(c),
        }
    }
    flush!();
    out
}

/// Redact secrets across segment boundaries: detect on the joined plain
/// text, then splice each finding's token into the segment where it starts
/// (dropping the secret's bytes from every segment it spans). Loops to a
/// fixed point like [`Detectors::redact_text`].
pub fn redact_segments(segments: Vec<Segment>, detectors: &Detectors) -> Vec<Segment> {
    let mut current = segments;
    loop {
        let joined: String = current.iter().map(|(t, _)| t.as_str()).collect();
        let findings = detectors.detect(&joined);
        if findings.is_empty() {
            return current;
        }
        let mut next: Vec<Segment> = Vec::new();
        let mut pos = 0usize; // byte offset of the current segment's start
        let mut fi = 0usize;
        for (text, class) in &current {
            let seg_start = pos;
            let seg_end = pos + text.len();
            let mut cursor = seg_start;
            let mut piece = String::new();
            while cursor < seg_end {
                match findings.get(fi) {
                    Some(f) if f.start < seg_end && f.end > cursor => {
                        if f.start >= cursor {
                            piece.push_str(&joined[cursor..f.start]);
                            piece.push_str(&hh_core::redact::replacement(&f.kind, &f.hash8));
                        }
                        // Skip the secret's bytes inside this segment.
                        cursor = f.end.min(seg_end);
                        if f.end <= seg_end {
                            fi += 1;
                        }
                    }
                    _ => {
                        piece.push_str(&joined[cursor..seg_end]);
                        cursor = seg_end;
                    }
                }
            }
            push_text(&mut next, &piece, class);
            pos = seg_end;
        }
        current = next;
    }
}

/// Keep at most `max_bytes` of segment text: the head and tail halves
/// around a marker segment. `0` means unlimited. Cuts land on `char`
/// boundaries. Returns the number of bytes dropped.
pub fn truncate_segments(segments: &mut Vec<Segment>, max_bytes: usize) -> usize {
    let total = plain_len(segments);
    if max_bytes == 0 || total <= max_bytes {
        return 0;
    }
    let head_budget = max_bytes / 2;
    let tail_budget = max_bytes - head_budget;

    let mut head: Vec<Segment> = Vec::new();
    let mut used = 0usize;
    for (t, c) in segments.iter() {
        if used >= head_budget {
            break;
        }
        let take = floor_char_boundary(t, head_budget - used);
        push_text(&mut head, &t[..take], c);
        used += take;
    }
    let mut tail: Vec<Segment> = Vec::new();
    let mut used_tail = 0usize;
    for (t, c) in segments.iter().rev() {
        if used_tail >= tail_budget {
            break;
        }
        let want = tail_budget - used_tail;
        let start = if t.len() <= want {
            0
        } else {
            ceil_char_boundary(t, t.len() - want)
        };
        tail.push((t[start..].to_string(), c.clone()));
        used_tail += t.len() - start;
    }
    tail.reverse();

    let dropped = total - used - used_tail;
    let mut out = head;
    out.push((
        format!(
            "\n… {} truncated — raise [export] html_terminal_max_bytes in config.toml to keep more …\n",
            human_bytes(dropped)
        ),
        "hh-t-marker".into(),
    ));
    for (t, c) in tail {
        push_text(&mut out, &t, &c);
    }
    *segments = out;
    dropped
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// `1536` → `"1.5 KiB"`.
fn human_bytes(n: usize) -> String {
    #[allow(clippy::cast_precision_loss)] // display only; sub-byte precision irrelevant
    let f = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KiB", f / 1024.0)
    } else {
        format!("{:.1} MiB", f / (1024.0 * 1024.0))
    }
}

/// Redact (when `detectors` is `Some`) and then truncate the payload's
/// `terminal` transcript in place. Order matters: redaction runs on the
/// complete text first so truncation can never split a secret.
pub fn finalize_payload_terminal(
    payload: &mut serde_json::Value,
    detectors: Option<&Detectors>,
    max_bytes: u64,
) {
    let Some(term) = payload.get_mut("terminal").filter(|t| t.is_object()) else {
        return;
    };
    let mut segments = segments_from_json(&term["segments"]);
    if let Some(d) = detectors {
        segments = redact_segments(segments, d);
    }
    let max = usize::try_from(max_bytes).unwrap_or(usize::MAX);
    let dropped = truncate_segments(&mut segments, max);
    term["truncated_bytes"] = serde_json::json!(dropped);
    term["segments"] = segments_to_json(&segments);
}

/// Fuzz entry point (`fuzz/fuzz_targets/html_terminal.rs`): runs arbitrary
/// text through the full pipeline — parse, redact, truncate — and checks
/// the output invariants. Panics only on an invariant violation (that is
/// the bug the fuzzer reports), never on malformed input.
///
/// # Panics
/// If a segment carries a control byte other than `\n`/`\t`, or a class
/// outside the `hh-t-*` vocabulary.
#[doc(hidden)]
pub fn fuzz_pipeline(raw: &str, detectors: &Detectors, max_bytes: usize) {
    let mut segs = redact_segments(parse_ansi(raw), detectors);
    truncate_segments(&mut segs, max_bytes);
    for (t, c) in &segs {
        assert!(t
            .chars()
            .all(|ch| ch == '\n' || ch == '\t' || !ch.is_control()));
        assert!(c.is_empty() || c.split(' ').all(|tok| tok.starts_with("hh-t-")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(segments: &[Segment]) -> String {
        segments.iter().map(|(t, _)| t.as_str()).collect()
    }

    #[test]
    fn sgr_becomes_classes_and_other_escapes_are_dropped() {
        let raw = "\u{1b}]0;evil title\u{7}\u{1b}[1;31mERR\u{1b}[0m ok\u{1b}[2K\u{1b}[10;5H\n\
                   \u{1b}]8;;https://x.invalid\u{1b}\\link\u{1b}]8;;\u{1b}\\ \u{1b}(Bdone\u{0}\u{7f}";
        let segs = parse_ansi(raw);
        assert_eq!(plain(&segs), "ERR ok\nlink done");
        assert_eq!(segs[0], ("ERR".into(), "hh-t-f1 hh-t-b".into()));
        assert!(segs.iter().all(|(t, _)| !t.contains('\u{1b}')));
        assert!(!plain(&segs).contains("https://"));
    }

    #[test]
    fn carriage_return_rewinds_and_backspace_erases() {
        let segs = parse_ansi("a\r\nprogress 10%\rprogress 100%\nab\u{8}c\n");
        assert_eq!(plain(&segs), "a\nprogress 100%\nac\n");
    }

    #[test]
    fn extended_colors_map_only_the_base_palette() {
        let segs = parse_ansi("\u{1b}[38;5;9mA\u{1b}[38;5;200mB\u{1b}[38;2;1;2;3mC\u{1b}[97;104mD");
        assert_eq!(segs[0], ("A".into(), "hh-t-f9".into()));
        assert_eq!(segs[1], ("BC".into(), String::new()));
        assert_eq!(segs[2], ("D".into(), "hh-t-f15 hh-t-g12".into()));
    }

    /// Never panics, and never emits an ESC/control byte, on arbitrary
    /// input — the parser consumes untrusted bytes.
    #[test]
    fn parser_is_total_on_hostile_input() {
        let mut seed: u32 = 0x9e37_79b9;
        let alphabet = [
            '\u{1b}', '[', ']', ';', 'm', '\r', '\n', '\u{8}', '\u{9b}', '\u{7}', '\\', '3', '8',
            '5', 'P', 'a', 'é', '\u{0}', '(', '\u{9c}',
        ];
        for _ in 0..2000 {
            let mut s = String::new();
            for _ in 0..(seed % 64) {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                s.push(alphabet[(seed as usize) % alphabet.len()]);
            }
            let segs = parse_ansi(&s);
            for (t, c) in &segs {
                assert!(t
                    .chars()
                    .all(|ch| ch == '\n' || ch == '\t' || !ch.is_control()));
                assert!(c
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == ' '));
            }
        }
    }

    #[test]
    fn redaction_catches_secrets_split_across_styles() {
        let d = Detectors::new(&hh_core::RedactionConfig::default()).unwrap();
        // The AWS key id is split by a color change mid-token.
        let segs = parse_ansi("key: AKIAIOSF\u{1b}[32mODNN7EXAMPLE\u{1b}[0m end\n");
        let red = redact_segments(segs, &d);
        let text = plain(&red);
        assert!(!text.contains("AKIAIOSFODNN7EXAMPLE"), "{text}");
        assert!(
            !text.contains("ODNN7EXAMPLE"),
            "tail half must go too: {text}"
        );
        assert!(text.contains("{{REDACTED:"), "{text}");
        assert!(
            text.starts_with("key: ") && text.ends_with(" end\n"),
            "{text}"
        );
    }

    #[test]
    fn truncation_keeps_head_and_tail_with_a_marker() {
        let mut segs = vec![("é".repeat(1000), String::new())]; // 2000 bytes
        let dropped = truncate_segments(&mut segs, 101);
        let text = plain(&segs);
        assert!(dropped > 0);
        assert!(text.contains("truncated"), "{text}");
        assert!(segs.iter().any(|(_, c)| c == "hh-t-marker"));
        assert!(text.starts_with('é') && text.ends_with('é'));
        // Unlimited and under-budget are no-ops.
        let mut segs2 = vec![("abc".to_string(), String::new())];
        assert_eq!(truncate_segments(&mut segs2, 0), 0);
        assert_eq!(truncate_segments(&mut segs2, 3), 0);
        assert_eq!(plain(&segs2), "abc");
    }

    #[test]
    fn transcript_skips_input_and_counts_binary_chunks() {
        let mut t = Transcript::default();
        t.push_chunk(Some(
            &serde_json::json!({"text": "out\n", "encoding": "utf8"}),
        ));
        t.push_chunk(Some(
            &serde_json::json!({"text": "hunter2", "encoding": "utf8", "direction": "input"}),
        ));
        t.push_chunk(Some(&serde_json::json!({"encoding": "blob", "size": 40})));
        let v = t.finish().unwrap();
        assert_eq!(v["segments"], serde_json::json!([["out\n", ""]]));
        assert_eq!(v["omitted_binary_bytes"], 40);
        assert!(Transcript::default().finish().is_none());
    }
}
