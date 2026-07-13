//! Secret detection and redaction (docs/redaction-design.md).
//!
//! A [`Detectors`] set runs pluggable detectors — compiled regexes for
//! high-signal named token types, a conservative high-entropy scanner, and
//! user-defined rules from config `[redaction] rules` — over any UTF-8 text.
//! Matches are replaced with `{{REDACTED:<kind>:<hash8>}}`, where `hash8` is
//! the first 8 hex chars of the secret's BLAKE3 hash: one-way, but stable, so
//! the same secret correlates across events and sessions without ever being
//! stored.
//!
//! Contract (property-tested, fuzzed): **false positives are acceptable;
//! a false negative on a named token type is a bug.** Detection never panics
//! on arbitrary input, and redacted output is a fixed point (redacting it
//! again finds nothing).

use crate::config::RedactionConfig;
use crate::error::{ConfigError, Result};
use regex::Regex;
use std::fmt;

/// Minimum length of a charset run the entropy detector will consider.
/// 40 = the length of an AWS secret access key, the canonical high-entropy
/// secret; anything shorter is too noisy to flag without a named pattern.
const ENTROPY_MIN_LEN: usize = 40;

/// Minimum Shannon entropy (bits per char) for a run to be flagged. Random
/// base64 at 40 chars measures ≈ 4.8; long camelCase identifiers measure
/// ≈ 4.0–4.4. 4.5 catches real keys with margin while staying conservative.
const ENTROPY_MIN_BITS: f64 = 4.5;

/// The kind of secret a detector found. `Display` yields the stable slug
/// used inside the replacement token and in `hh scan` output.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SecretKind {
    /// AWS access key id (`AKIA…`/`ASIA…` and friends).
    AwsAccessKeyId,
    /// GitHub token (`ghp_`/`gho_`/`ghu_`/`ghs_`/`ghr_`/`github_pat_`).
    GithubToken,
    /// GitLab token (`glpat-`/`glrt-`/`gldt-`/`glsoat-`/`glcbt-`).
    GitlabToken,
    /// Slack token (`xoxb-`/`xoxa-`/`xoxp-`/`xoxr-`/`xoxs-`/`xoxe-`).
    SlackToken,
    /// A PEM private-key block (`-----BEGIN … PRIVATE KEY-----`).
    PrivateKey,
    /// A JSON Web Token (three base64url segments, `eyJ…`-headed).
    Jwt,
    /// A generic high-entropy string above the conservative threshold.
    HighEntropy,
    /// A user-defined rule from config `[redaction] rules`; reports as
    /// `custom:<name>`.
    Custom(String),
}

impl fmt::Display for SecretKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AwsAccessKeyId => f.write_str("aws-access-key-id"),
            Self::GithubToken => f.write_str("github-token"),
            Self::GitlabToken => f.write_str("gitlab-token"),
            Self::SlackToken => f.write_str("slack-token"),
            Self::PrivateKey => f.write_str("private-key"),
            Self::Jwt => f.write_str("jwt"),
            Self::HighEntropy => f.write_str("high-entropy"),
            Self::Custom(name) => write!(f, "custom:{name}"),
        }
    }
}

/// One detected secret: its kind, byte span within the scanned text, and the
/// 8-hex-char BLAKE3 prefix of the secret bytes (see [`hash8`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What matched.
    pub kind: SecretKind,
    /// Byte offset of the match start within the scanned text.
    pub start: usize,
    /// Byte offset of the match end (exclusive).
    pub end: usize,
    /// `BLAKE3(secret)[..8]` — correlates the same secret across findings
    /// without storing it.
    pub hash8: String,
}

/// The result of redacting a text: the rewritten text plus every finding
/// that was replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedText {
    /// The text with every match replaced by its redaction token.
    pub text: String,
    /// The findings that were replaced, in text order.
    pub findings: Vec<Finding>,
}

/// The result of redacting raw bytes (see [`Detectors::redact_bytes`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedBytes {
    /// The rewritten content.
    pub bytes: Vec<u8>,
    /// The findings that were replaced.
    pub findings: Vec<Finding>,
}

/// First 8 hex chars of `BLAKE3(secret)`. One-way (32 bits of a hash of a
/// high-entropy input) but stable: the same secret yields the same tag
/// everywhere, so a user can trace one credential across events without
/// ever seeing it.
#[must_use]
pub fn hash8(secret: &[u8]) -> String {
    let hex = blake3::hash(secret).to_hex();
    hex.as_str()[..8].to_string()
}

/// Build the replacement token `{{REDACTED:<kind>:<hash8>}}`. The token is a
/// detection fixed point: the `:`/`{`/`}` separators break any charset run,
/// and no built-in pattern matches the slugs, so redacted output never
/// re-triggers a detector.
#[must_use]
pub fn replacement(kind: &SecretKind, hash8: &str) -> String {
    format!("{{{{REDACTED:{kind}:{hash8}}}}}")
}

/// A compiled detector set. Construct once per process from the loaded
/// config ([`Detectors::new`]) and share via `Arc`; detection itself is
/// `&self` and thread-safe.
#[derive(Debug)]
pub struct Detectors {
    /// `(kind, compiled regex)` for the named built-ins + custom rules.
    named: Vec<(SecretKind, Regex)>,
    /// Whether the high-entropy scanner runs.
    entropy: bool,
}

impl Detectors {
    /// Compile the built-in detectors plus the user rules from `cfg`.
    ///
    /// # Errors
    ///
    /// An invalid user `pattern` is an actionable [`ConfigError::Value`]
    /// naming the rule — a detector that silently fails to compile would be
    /// a redaction hole.
    pub fn new(cfg: &RedactionConfig) -> Result<Self> {
        let mut named = built_in_detectors()?;
        for rule in &cfg.rules {
            let re = Regex::new(&rule.pattern).map_err(|e| {
                ConfigError::Value(format!(
                    "redaction rule `{}` has an invalid regex: {e}\n  \
                     hint: patterns use Rust `regex` syntax (no backreferences/lookaround)",
                    rule.name
                ))
            })?;
            named.push((SecretKind::Custom(rule.name.clone()), re));
        }
        Ok(Self {
            named,
            entropy: cfg.entropy,
        })
    }

    /// Run every detector over `text`, returning non-overlapping findings in
    /// text order. Where a named match and an entropy run overlap, the named
    /// match wins when they start together; otherwise the earlier span wins
    /// (the secret is still covered either way).
    #[must_use]
    pub fn detect(&self, text: &str) -> Vec<Finding> {
        let mut findings = Vec::new();
        for (kind, re) in &self.named {
            for m in re.find_iter(text) {
                findings.push(Finding {
                    kind: kind.clone(),
                    start: m.start(),
                    end: m.end(),
                    hash8: hash8(m.as_str().as_bytes()),
                });
            }
        }
        if self.entropy {
            for (start, end) in entropy_spans(text) {
                findings.push(Finding {
                    kind: SecretKind::HighEntropy,
                    start,
                    end,
                    hash8: hash8(&text.as_bytes()[start..end]),
                });
            }
        }
        resolve_overlaps(findings)
    }

    /// Redact `text`, replacing every finding with its token. Returns `None`
    /// when the text is clean (no allocation, no rewrite). The returned text
    /// is a fixed point: re-detecting it finds nothing (the loop below runs
    /// until clean; each pass strictly shrinks the surviving original text,
    /// and replacement tokens never re-match, so it terminates).
    #[must_use]
    pub fn redact_text(&self, text: &str) -> Option<RedactedText> {
        let mut findings = self.detect(text);
        if findings.is_empty() {
            return None;
        }
        let mut current = apply(text, &findings);
        // Overlap resolution can leave a tail that only becomes a detectable
        // run once its overlapping neighbor is replaced (e.g. an entropy run
        // that started inside a dropped, longer finding). Re-scan until clean.
        loop {
            let more = self.detect(&current);
            if more.is_empty() {
                return Some(RedactedText {
                    text: current,
                    findings,
                });
            }
            current = apply(&current, &more);
            findings.extend(more);
        }
    }

    /// Redact every string scalar in a JSON tree in place, returning the
    /// findings (spans are relative to each individual string). Structured
    /// payloads must be redacted on the *parsed* tree, not the encoded text,
    /// for two reasons: `hash8` must be computed over the raw secret bytes
    /// (a hash over the `\n`-escaped encoding would not correlate with the
    /// same secret seen in plain text), and splicing tokens into encoded
    /// text could split an escape sequence and corrupt the stored JSON.
    /// Object keys are not rewritten (they are structural, and a
    /// secret-as-key does not occur in data Halfhand writes).
    pub fn redact_json(&self, value: &mut serde_json::Value) -> Vec<Finding> {
        match value {
            serde_json::Value::String(s) => match self.redact_text(s) {
                Some(r) => {
                    *s = r.text;
                    r.findings
                }
                None => Vec::new(),
            },
            serde_json::Value::Array(items) => {
                items.iter_mut().flat_map(|v| self.redact_json(v)).collect()
            }
            serde_json::Value::Object(map) => {
                map.values_mut().flat_map(|v| self.redact_json(v)).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Detect over a JSON tree without mutating it (the `hh scan` path).
    #[must_use]
    pub fn detect_json(&self, value: &serde_json::Value) -> Vec<Finding> {
        match value {
            serde_json::Value::String(s) => self.detect(s),
            serde_json::Value::Array(items) => {
                items.iter().flat_map(|v| self.detect_json(v)).collect()
            }
            serde_json::Value::Object(map) => {
                map.values().flat_map(|v| self.detect_json(v)).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Redact raw content (a blob): JSON-aware when the bytes parse as JSON
    /// (see [`Self::redact_json`] for why), plain text otherwise. Returns
    /// `None` when the content is clean or not UTF-8 (binary content is not
    /// scanned — a documented limitation).
    #[must_use]
    pub fn redact_bytes(&self, content: &[u8]) -> Option<RedactedBytes> {
        if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(content) {
            let findings = self.redact_json(&mut v);
            if findings.is_empty() {
                return None;
            }
            let bytes = serde_json::to_vec(&v).ok()?;
            return Some(RedactedBytes { bytes, findings });
        }
        let text = std::str::from_utf8(content).ok()?;
        self.redact_text(text).map(|r| RedactedBytes {
            bytes: r.text.into_bytes(),
            findings: r.findings,
        })
    }

    /// Detect over raw content without mutating it (the `hh scan` path):
    /// JSON-aware when it parses, plain text when UTF-8, skipped otherwise.
    #[must_use]
    pub fn detect_bytes(&self, content: &[u8]) -> Vec<Finding> {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(content) {
            return self.detect_json(&v);
        }
        match std::str::from_utf8(content) {
            Ok(text) => self.detect(text),
            Err(_) => Vec::new(),
        }
    }
}

/// Compile the built-in named detectors. These patterns are the crate's
/// "false negatives are bugs" contract — each is locked by a unit test and a
/// property test in this module.
fn built_in_detectors() -> Result<Vec<(SecretKind, Regex)>> {
    let table: &[(SecretKind, &str)] = &[
        (
            SecretKind::AwsAccessKeyId,
            r"\b(?:A3T[A-Z0-9]|AKIA|AGPA|AIDA|AROA|AIPA|ANPA|ANVA|ASIA|ABIA|ACCA)[A-Z0-9]{16}\b",
        ),
        (
            SecretKind::GithubToken,
            r"\b(?:gh[pousr]_[A-Za-z0-9]{36,255}|github_pat_[A-Za-z0-9_]{22,255})\b",
        ),
        (
            SecretKind::GitlabToken,
            r"\bgl(?:pat|rt|dt|soat|cbt)-[0-9A-Za-z_=\-]{20,100}\b",
        ),
        (
            SecretKind::SlackToken,
            r"\bxox[abeprs]-[0-9A-Za-z\-]{10,250}\b",
        ),
        (
            SecretKind::PrivateKey,
            // (?s): PEM blocks span lines. Non-greedy body: the match ends at
            // the *first* END marker.
            r"(?s)-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----.*?-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----",
        ),
        (
            SecretKind::PrivateKey,
            // Truncated-PEM fallback: a BEGIN header followed by base64-ish
            // body lines but no END marker — what a PEM looks like after the
            // 120-char summary truncation cut it off. Each body run must be
            // ≥16 chars so trailing prose (short words) is not swallowed;
            // when a complete block is present the pattern above matches a
            // superset and wins overlap resolution.
            r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----(?:\s+[A-Za-z0-9+/=_-]{16,})+",
        ),
        (
            SecretKind::Jwt,
            r"\bey[A-Za-z0-9_\-]{14,}\.ey[A-Za-z0-9_\-]{14,}\.[A-Za-z0-9_\-]{10,}\b",
        ),
    ];
    let mut out = Vec::with_capacity(table.len());
    for (kind, pattern) in table {
        // Built-in patterns are compile-time constants; a failure here is a
        // programming error, but per CLAUDE.md we still surface it as an
        // error rather than unwrap.
        let re = Regex::new(pattern).map_err(|e| {
            ConfigError::Value(format!(
                "internal: built-in pattern for {kind} invalid: {e}"
            ))
        })?;
        out.push((kind.clone(), re));
    }
    Ok(out)
}

/// Replace each finding's span with its token. `findings` must be sorted and
/// non-overlapping ([`resolve_overlaps`] guarantees both).
fn apply(text: &str, findings: &[Finding]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pos = 0;
    for f in findings {
        out.push_str(&text[pos..f.start]);
        out.push_str(&replacement(&f.kind, &f.hash8));
        pos = f.end;
    }
    out.push_str(&text[pos..]);
    out
}

/// Sort findings and drop overlaps. Order of preference at the same start:
/// named kinds before `high-entropy` (a named match is more specific and its
/// hash8 correlates the exact token), then longer matches first. Across
/// different starts, the earlier span wins — the later overlapping one is
/// dropped for this pass (the redact loop re-scans, so nothing is lost).
fn resolve_overlaps(mut findings: Vec<Finding>) -> Vec<Finding> {
    findings.sort_by(|a, b| {
        a.start
            .cmp(&b.start)
            .then_with(|| entropy_rank(a).cmp(&entropy_rank(b)))
            .then_with(|| b.end.cmp(&a.end))
    });
    let mut out: Vec<Finding> = Vec::with_capacity(findings.len());
    for f in findings {
        // MSRV 1.75: `Option::is_none_or` is not available yet.
        if out.last().map_or(true, |last| f.start >= last.end) {
            out.push(f);
        }
    }
    out
}

/// Sort rank for overlap preference: named/custom detectors outrank the
/// generic entropy scanner.
fn entropy_rank(f: &Finding) -> u8 {
    u8::from(f.kind == SecretKind::HighEntropy)
}

/// Byte predicate for the entropy scanner's token charset: base64 (standard
/// + url-safe), hex, and the `=`/`_`/`-` fillers that appear in real keys.
fn is_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'-')
}

/// Find maximal charset runs ≥ [`ENTROPY_MIN_LEN`] whose Shannon entropy
/// clears [`ENTROPY_MIN_BITS`]. Deliberately conservative:
/// - pure-hex runs are skipped — Halfhand's own BLAKE3 hashes (64 hex) and
///   git SHAs appear throughout recorded data and must never be redacted or
///   blob references would break;
/// - runs need both letters and digits (filters padding, dashes, prose).
///
/// The charset is pure ASCII, so run boundaries are always char boundaries
/// and the byte offsets are safe to slice with.
fn entropy_spans(text: &str) -> Vec<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !is_token_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_token_byte(bytes[i]) {
            i += 1;
        }
        let run = &text[start..i];
        if run.len() >= ENTROPY_MIN_LEN && qualifies_as_entropy(run) {
            spans.push((start, i));
        }
    }
    spans
}

/// The entropy qualifier for one charset run (see [`entropy_spans`]).
fn qualifies_as_entropy(run: &str) -> bool {
    if run.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let has_digit = run.bytes().any(|b| b.is_ascii_digit());
    let has_alpha = run.bytes().any(|b| b.is_ascii_alphabetic());
    if !(has_digit && has_alpha) {
        return false;
    }
    shannon_bits_per_char(run) >= ENTROPY_MIN_BITS
}

/// Shannon entropy of a string in bits per char.
#[allow(clippy::cast_precision_loss)] // counts are ≤ text length, far inside f64's mantissa
fn shannon_bits_per_char(s: &str) -> f64 {
    let mut counts = [0usize; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / n;
            -p * p.log2()
        })
        .sum()
}

/// Fuzz-only entry points (`cargo fuzz` target `redact_detect`). Gated behind
/// the `fuzzing` feature so it never widens the crate's normal public API.
#[cfg(feature = "fuzzing")]
pub mod fuzzing {
    use super::Detectors;
    use crate::config::RedactionConfig;
    use std::sync::OnceLock;

    fn detectors() -> &'static Detectors {
        static D: OnceLock<Detectors> = OnceLock::new();
        D.get_or_init(|| {
            Detectors::new(&RedactionConfig::default())
                .unwrap_or_else(|_| unreachable!("built-in detectors compile"))
        })
    }

    /// Fuzz the whole engine on arbitrary text: detection must never panic,
    /// and redacted output must be a fixed point (no findings on re-scan).
    pub fn fuzz_detect_and_redact(text: &str) {
        let d = detectors();
        let _ = d.detect(text);
        if let Some(r) = d.redact_text(text) {
            assert!(
                d.detect(&r.text).is_empty(),
                "redacted output must be a detection fixed point"
            );
        }
        // The byte path (JSON-aware) must be panic-free too.
        let _ = d.redact_bytes(text.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RedactionConfig, RedactionRule};

    fn detectors() -> Detectors {
        Detectors::new(&RedactionConfig::default()).unwrap()
    }

    /// Each named token type is detected verbatim and classified correctly —
    /// the "false negatives on named types are bugs" contract, example form.
    /// (The property form with random surroundings lives in
    /// tests/prop_redact.rs.)
    #[test]
    fn named_types_are_detected_and_classified() {
        let d = detectors();
        let cases: &[(&str, SecretKind)] = &[
            ("AKIAIOSFODNN7EXAMPLE", SecretKind::AwsAccessKeyId),
            ("ASIAIOSFODNN7EXAMPLE", SecretKind::AwsAccessKeyId),
            (
                "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA1",
                SecretKind::GithubToken,
            ),
            (
                "ghs_1234567890abcdefghijklmnopqrstuvwxyzAB",
                SecretKind::GithubToken,
            ),
            (
                "github_pat_11AAAAAAA0aaaaaaaaaaaaaa_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                SecretKind::GithubToken,
            ),
            ("glpat-xxxxxxxxxxxxxxxxxxxx", SecretKind::GitlabToken),
            (
                "xoxb-notarealtoken-placeholder-value-fixture",
                SecretKind::SlackToken,
            ),
            (
                "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
                SecretKind::Jwt,
            ),
        ];
        for (secret, want_kind) in cases {
            let text = format!("before {secret} after");
            let findings = d.detect(&text);
            assert_eq!(findings.len(), 1, "expected one finding in: {text}");
            let f = &findings[0];
            assert_eq!(&f.kind, want_kind, "kind for {secret}");
            assert_eq!(&text[f.start..f.end], *secret, "span must be the token");
            assert_eq!(f.hash8, hash8(secret.as_bytes()));
        }
    }

    #[test]
    fn pem_private_key_block_is_detected_across_lines() {
        let d = detectors();
        let pem = "-----BEGIN RSA PRIVATE KEY-----\n\
                   MIIEpAIBAAKCAQEA7bq0\n\
                   u3+fake+key+material+lines\n\
                   -----END RSA PRIVATE KEY-----";
        let text = format!("prefix\n{pem}\nsuffix");
        let findings = d.detect(&text);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, SecretKind::PrivateKey);
        assert_eq!(&text[findings[0].start..findings[0].end], pem);
        // Also OPENSSH / unlabeled variants.
        for label in ["OPENSSH PRIVATE KEY", "EC PRIVATE KEY", "PRIVATE KEY"] {
            let block = format!("-----BEGIN {label}-----\nabc\n-----END {label}-----");
            assert_eq!(
                d.detect(&block).first().map(|f| f.kind.clone()),
                Some(SecretKind::PrivateKey),
                "label {label}"
            );
        }
    }

    #[test]
    fn entropy_flags_random_base64_but_not_hex_hashes_or_prose() {
        let d = detectors();
        // A 40-char AWS-secret-shaped random base64 string.
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYzEXAMPLEKEY3";
        let findings = d.detect(secret);
        assert_eq!(
            findings.first().map(|f| f.kind.clone()),
            Some(SecretKind::HighEntropy),
            "random base64 must be flagged: {findings:?}"
        );
        // A BLAKE3 hash (64 lowercase hex) must never be flagged — blob
        // references appear throughout recorded data.
        let hash = blake3::hash(b"anything").to_hex().to_string();
        assert!(d.detect(&hash).is_empty(), "hex hash must not be flagged");
        // Prose and paths must not be flagged.
        for clean in [
            "the quick brown fox jumps over the lazy dog repeatedly today",
            "/home/user/projects/halfhand/hh-core/src/redact.rs",
            "cargo clippy --workspace --all-targets -- -D warnings",
        ] {
            assert!(d.detect(clean).is_empty(), "false positive on: {clean}");
        }
    }

    #[test]
    fn entropy_can_be_disabled() {
        let d = Detectors::new(&RedactionConfig {
            entropy: false,
            ..RedactionConfig::default()
        })
        .unwrap();
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYzEXAMPLEKEY3";
        assert!(d.detect(secret).is_empty());
        // Named types still fire.
        assert!(!d.detect("AKIAIOSFODNN7EXAMPLE").is_empty());
    }

    #[test]
    fn custom_rules_report_as_custom_kind() {
        let d = Detectors::new(&RedactionConfig {
            rules: vec![RedactionRule {
                name: "acme".into(),
                pattern: "ACME-[0-9A-F]{16}".into(),
            }],
            ..RedactionConfig::default()
        })
        .unwrap();
        let findings = d.detect("token ACME-0123456789ABCDEF here");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, SecretKind::Custom("acme".into()));
        assert_eq!(findings[0].kind.to_string(), "custom:acme");
    }

    #[test]
    fn invalid_custom_rule_is_an_actionable_error() {
        let err = Detectors::new(&RedactionConfig {
            rules: vec![RedactionRule {
                name: "bad".into(),
                pattern: "(unclosed".into(),
            }],
            ..RedactionConfig::default()
        })
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bad"), "must name the rule: {msg}");
        assert!(msg.contains("hint"), "must carry a hint: {msg}");
    }

    #[test]
    fn redact_replaces_with_token_and_is_fixed_point() {
        let d = detectors();
        let secret = "AKIAIOSFODNN7EXAMPLE";
        let text = format!("aws_access_key_id = {secret}\n");
        let r = d.redact_text(&text).expect("must redact");
        let want_token = replacement(&SecretKind::AwsAccessKeyId, &hash8(secret.as_bytes()));
        assert!(r.text.contains(&want_token), "got: {}", r.text);
        assert!(!r.text.contains(secret), "secret must be gone");
        assert!(
            d.redact_text(&r.text).is_none(),
            "redacted output must be a fixed point"
        );
    }

    #[test]
    fn same_secret_gets_same_hash8_everywhere() {
        let d = detectors();
        let secret = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA1";
        let a = d.detect(&format!("x {secret} y")).remove(0);
        let b = d.detect(&format!("entirely different {secret}")).remove(0);
        assert_eq!(a.hash8, b.hash8);
    }

    #[test]
    fn named_match_wins_over_entropy_at_same_start() {
        let d = detectors();
        // ghp_ + 36 chars = a 40-char charset run: both detectors fire.
        let secret = "ghp_AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        let findings = d.detect(secret);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, SecretKind::GithubToken);
    }

    #[test]
    fn redact_json_walks_nested_strings_and_catches_escaped_pem() {
        let d = detectors();
        let pem = "-----BEGIN PRIVATE KEY-----\nsecret\n-----END PRIVATE KEY-----";
        let mut v = serde_json::json!({
            "tool": "Write",
            "input": { "content": pem, "list": ["clean", "AKIAIOSFODNN7EXAMPLE"] },
            "n": 42,
        });
        let raw_pem_hash8 = hash8(pem.as_bytes());
        let findings = d.redact_json(&mut v);
        assert!(findings.iter().any(|f| f.kind == SecretKind::PrivateKey));
        assert!(findings
            .iter()
            .any(|f| f.kind == SecretKind::AwsAccessKeyId));
        let out = serde_json::to_string(&v).unwrap();
        assert!(!out.contains("BEGIN PRIVATE"), "pem gone: {out}");
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "aws key gone: {out}");
        // hash8 is computed over the *raw* secret (with real newlines), so it
        // correlates with the same PEM seen in plain text elsewhere.
        assert!(
            out.contains(&format!("{{{{REDACTED:private-key:{raw_pem_hash8}}}}}")),
            "token must carry the raw-bytes hash8: {out}"
        );
    }

    #[test]
    fn redact_bytes_is_json_aware_and_skips_binary() {
        let d = detectors();
        // JSON blob (an overflowed body): tree-walked.
        let blob = serde_json::to_vec(&serde_json::json!({
            "content": "key AKIAIOSFODNN7EXAMPLE end"
        }))
        .unwrap();
        let r = d.redact_bytes(&blob).expect("must redact");
        let rewritten: serde_json::Value = serde_json::from_slice(&r.bytes).unwrap();
        assert!(!rewritten.to_string().contains("AKIAIOSFODNN7EXAMPLE"));
        // Plain-text blob.
        let r2 = d.redact_bytes(b"AKIAIOSFODNN7EXAMPLE").expect("plain text");
        assert!(!String::from_utf8_lossy(&r2.bytes).contains("AKIAIOSFODNN7EXAMPLE"));
        // Binary: skipped (None), never panics.
        assert!(d.redact_bytes(&[0u8, 159, 146, 150]).is_none());
        // Clean text: None (no rewrite).
        assert!(d.redact_bytes(b"nothing to see here").is_none());
    }

    #[test]
    fn multiple_and_adjacent_secrets_all_replaced() {
        let d = detectors();
        let s1 = "AKIAIOSFODNN7EXAMPLE";
        let s2 = "xoxb-notarealtoken-placeholder-value-fixture";
        let text = format!("{s1} {s2} and again {s1}");
        let r = d.redact_text(&text).unwrap();
        assert!(!r.text.contains(s1) && !r.text.contains(s2));
        assert_eq!(r.findings.len(), 3);
        // The two s1 findings share a hash8.
        let h: Vec<&str> = r
            .findings
            .iter()
            .filter(|f| f.kind == SecretKind::AwsAccessKeyId)
            .map(|f| f.hash8.as_str())
            .collect();
        assert_eq!(h[0], h[1]);
    }

    #[test]
    fn replacement_token_shape() {
        let t = replacement(&SecretKind::SlackToken, "a1b2c3d4");
        assert_eq!(t, "{{REDACTED:slack-token:a1b2c3d4}}");
        assert_eq!(hash8(b"x").len(), 8);
    }
}
