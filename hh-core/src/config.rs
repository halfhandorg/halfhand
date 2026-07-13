//! Configuration and path resolution (SRS §2.3, §4.2).
//!
//! Precedence is **default < config file < `HH_DATA_DIR` env**. Unknown config
//! keys emit a single-line warning on stderr but never prevent startup, per
//! SRS §4.2 ("All keys optional; unknown keys warn, never fail").

use crate::error::{ConfigError, Result};
use serde::Deserialize;
use std::fmt;
use std::path::{Path, PathBuf};

/// Byte-size parser for values like `4MiB`, `512KiB`, `100B`.
///
/// Accepts `B`, `KB`/`KiB`, `MB`/`MiB`, `GB`/`GiB` (case-insensitive). Binary
/// (`KiB`) and decimal (`KB`) suffixes are both treated as binary multiples
/// (1024-based) for simplicity, matching how the rest of the tool talks about
/// file sizes; a bare integer is interpreted as bytes.
pub fn parse_bytes(input: &str) -> std::result::Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty byte size".into());
    }
    // Split into numeric prefix and suffix at the first non-digit/non-dot.
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let num_part = &s[..split];
    let suffix = s[split..].trim().to_ascii_lowercase();
    // Integer + optional fractional part, parsed without f64 to stay exact.
    let (int_part, frac_part) = match num_part.split_once('.') {
        Some((i, f)) => (i, f),
        None => (num_part, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return Err(format!("`{s}` is not a number"));
    }
    if !int_part.chars().all(|c| c.is_ascii_digit())
        || !frac_part.chars().all(|c| c.is_ascii_digit())
    {
        return Err(format!("`{s}` is not a number"));
    }
    let mult: u128 = match suffix.as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024 * 1024,
        "g" | "gb" | "gib" => 1024 * 1024 * 1024,
        other => return Err(format!("unknown byte-size suffix `{other}`")),
    };
    let int_val: u128 = int_part.parse().unwrap_or(0);
    let total = int_val
        .checked_mul(mult)
        .ok_or_else(|| "byte size too large".to_string())?;
    let frac_val: u128 = if frac_part.is_empty() {
        0
    } else {
        frac_part.parse().unwrap_or(0)
    };
    let denom = 10u128
        .checked_pow(u32::try_from(frac_part.len()).unwrap_or(0))
        .ok_or_else(|| "byte size too large".to_string())?;
    let frac_contrib = frac_val
        .checked_mul(mult)
        .ok_or_else(|| "byte size too large".to_string())?
        .checked_div(denom)
        .unwrap_or(0);
    let bytes = total
        .checked_add(frac_contrib)
        .ok_or_else(|| "byte size too large".to_string())?;
    u64::try_from(bytes).map_err(|_| "byte size too large".to_string())
}

/// Replay color theme (SRS §4.2 `[replay] theme`).
///
/// `#[non_exhaustive]`: this and the other config types below are the crate's
/// growth-prone public surface — new `[section]` keys/variants are expected
/// over time (CLAUDE.md v1.0.0 addendum: additive-only). Marking them
/// non-exhaustive up front means a future added key/variant stays additive
/// under `cargo-semver-checks --release-type minor` instead of registering as
/// a break, matching what the check is actually meant to enforce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[non_exhaustive]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow the terminal's reported color scheme.
    #[default]
    Auto,
    /// Force dark.
    Dark,
    /// Force light.
    Light,
}

/// Record-time options (SRS §4.2 `[record]`).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct RecordConfig {
    /// Max file size to capture, in bytes (default 4 MiB).
    pub max_file_size: u64,
    /// Whether to record user keystrokes (default off; NFR-4).
    pub record_input: bool,
    /// Whether to store binary file contents (default off).
    pub record_binary: bool,
    /// Extra ignore patterns extending the built-in list + .gitignore.
    pub ignore: Vec<String>,
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            max_file_size: 4 * 1024 * 1024,
            record_input: false,
            record_binary: false,
            ignore: Vec::new(),
        }
    }
}

/// Storage options (SRS §4.2 `[storage]`).
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct StorageConfig {
    /// Data directory override; empty means platform default.
    pub data_dir: PathBuf,
}

/// Replay options (SRS §4.2 `[replay]`).
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct ReplayConfig {
    /// Color theme.
    pub theme: Theme,
}

/// One user-defined secret detector (`[redaction] rules`, see
/// docs/redaction-design.md). The `pattern` is compiled by
/// [`crate::redact::Detectors::new`]; an invalid regex is an actionable error
/// there, not a silent no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RedactionRule {
    /// Short rule name; findings report as `custom:<name>`.
    pub name: String,
    /// The regex to match (Rust `regex` syntax, linear-time).
    pub pattern: String,
}

/// Redaction options (`[redaction]`, docs/redaction-design.md).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RedactionConfig {
    /// Redact matches *before they hit disk* during recording (opt-in;
    /// default off — sessions record raw locally, and export-time redaction
    /// guards what leaves the machine).
    pub at_record: bool,
    /// Enable the conservative high-entropy string detector (default on).
    pub entropy: bool,
    /// User-defined detectors, applied in addition to the built-ins.
    pub rules: Vec<RedactionRule>,
}

impl Default for RedactionConfig {
    fn default() -> Self {
        Self {
            at_record: false,
            entropy: true,
            rules: Vec::new(),
        }
    }
}

/// The full configuration.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Config {
    /// `[record]`
    pub record: RecordConfig,
    /// `[storage]`
    pub storage: StorageConfig,
    /// `[replay]`
    pub replay: ReplayConfig,
    /// `[redaction]`
    pub redaction: RedactionConfig,
}

impl Config {
    /// Load configuration from `config_path`, falling back to [`Default`] when
    /// the file does not exist. Unknown keys warn on stderr but never fail.
    ///
    /// When `config_path` (`config.toml`) is absent but a legacy config file
    /// (`halfhand.toml` or `hh.toml`) exists in the same directory, that file
    /// is loaded instead — a pre-rename config still takes effect and is never
    /// silently ignored. A single one-line deprecation hint is emitted on stderr
    /// pointing the user at the rename.
    pub fn load(config_path: &Path) -> Result<Self> {
        let mut cfg = Self::default();
        let table = match read_or_default_config(config_path)? {
            Some(table) => Some(table),
            None => match legacy_fallback_path(config_path) {
                Some(legacy) => {
                    crate::deprecation::warn_deprecated(
                        "legacy-config-filename",
                        &format!("the config filename `{}`", legacy.display()),
                        &format!(
                            "rename it to `{}` (loading `{}` for now so its settings still take effect)",
                            config_path.display(),
                            legacy.display(),
                        ),
                    );
                    read_or_default_config(&legacy)?
                }
                None => None,
            },
        };
        let Some(table) = table else {
            return Ok(cfg);
        };
        warn_unknown_keys(&table);
        merge_table(&mut cfg, &table)?;
        Ok(cfg)
    }
}

/// Other filenames users commonly reach for, in addition to the canonical
/// `config.toml`. When the canonical `config.toml` is *absent*, [`Config::load`]
/// falls back to the first of these that exists in the same directory (so a
/// pre-rename config still takes effect — it is never silently ignored). When
/// `config.toml` *is* present, these are genuinely ignored (the canonical path
/// wins), and silent misconfiguration (ignore globs never applied, a custom
/// data dir never honored) is a bug, so we warn loudly and tell the user
/// exactly what to move where.
const NONCANONICAL_CONFIG_NAMES: &[&str] = &["halfhand.toml", "hh.toml"];

/// The first existing legacy config file (e.g. `halfhand.toml`, `hh.toml`) in
/// the same directory as `config_path`, in [`NONCANONICAL_CONFIG_NAMES`] order,
/// or `None` when none is present. Callers are responsible for only treating
/// this as a fallback when the canonical `config.toml` is absent (the canonical
/// path always wins).
fn legacy_fallback_path(config_path: &Path) -> Option<PathBuf> {
    let dir = config_path.parent()?;
    NONCANONICAL_CONFIG_NAMES
        .iter()
        .map(|name| dir.join(name))
        .find(|c| c.exists())
}

/// Warn on stderr if a non-canonical config file (e.g. `halfhand.toml`) exists
/// alongside the canonical `config_path` (`config.toml`) — i.e. it is genuinely
/// being ignored because the canonical file is present. No warning is emitted
/// when `config.toml` is absent: in that case [`Config::load`] falls back to the
/// legacy file and loads it, so nothing is ignored. Idempotent and best-effort:
/// a missing parent dir or an unreadable file is silently skipped. Called by the
/// binary on every store open so the user learns their config is being ignored.
pub fn warn_on_ignored_config_files(config_path: &Path) {
    for candidate in ignored_noncanonical_config_files(config_path) {
        eprintln!(
            "hh: warning: found {cand} but Halfhand reads {canonical}; ignoring {cand} \
             — move its contents into {canonical} so they take effect",
            cand = candidate.display(),
            canonical = config_path.display(),
        );
    }
}

/// Return the non-canonical config files (e.g. `halfhand.toml`, `hh.toml`) that
/// are *genuinely ignored* alongside `config_path` — i.e. only when the
/// canonical `config.toml` is present (it wins). Empty when `config.toml` is
/// absent (then [`Config::load`] falls back to the legacy file and loads it, so
/// nothing is ignored) or when no legacy file is present (the common,
/// correctly-configured case). Used by `hh doctor` to report this class of
/// silent misconfiguration in its structured output, where the stderr warning
/// from [`warn_on_ignored_config_files`] would not be captured (e.g. `--json`).
#[must_use]
pub fn ignored_noncanonical_config_files(config_path: &Path) -> Vec<PathBuf> {
    // Legacy files are only "ignored" when the canonical config is present
    // (it wins). When it's absent, Config::load falls back to them, so they
    // are not ignored and must not be reported here.
    if !config_path.exists() {
        return Vec::new();
    }
    let Some(dir) = config_path.parent() else {
        return Vec::new();
    };
    NONCANONICAL_CONFIG_NAMES
        .iter()
        .map(|name| dir.join(name))
        .filter(|c| c.exists())
        .collect()
}

/// Read the config file into a TOML table, returning `Ok(None)` if it does not
/// exist (not an error — the file is entirely optional per SRS §4.2).
fn read_or_default_config(path: &Path) -> Result<Option<toml::Table>> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let table: toml::Table = toml::from_str(&contents).map_err(|e| ConfigError::Parse {
                path: path.to_path_buf(),
                source: e,
            })?;
            Ok(Some(table))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source: e,
        }
        .into()),
    }
}

/// Known top-level tables and their allowed keys.
const KNOWN: &[(&str, &[&str])] = &[
    (
        "record",
        &["max_file_size", "record_input", "record_binary", "ignore"],
    ),
    ("storage", &["data_dir"]),
    ("replay", &["theme"]),
    ("redaction", &["at_record", "entropy", "rules"]),
];

/// Walk the parsed table and warn on stderr about any key we do not recognize.
fn warn_unknown_keys(table: &toml::Table) {
    for (key, value) in table {
        let known_keys = KNOWN
            .iter()
            .find_map(|(k, keys)| (*k == key).then_some(*keys));
        match known_keys {
            None => eprintln!("warn: config: unknown top-level key `{key}` ignored"),
            Some(allowed) => {
                if let Some(sub) = value.as_table() {
                    for (subkey, _) in sub {
                        if !allowed.contains(&subkey.as_str()) {
                            eprintln!("warn: config: unknown key `{key}.{subkey}` ignored");
                        }
                    }
                }
            }
        }
    }
}

/// Merge a parsed TOML table into a [`Config`], interpreting known values.
fn merge_table(cfg: &mut Config, table: &toml::Table) -> Result<()> {
    if let Some(record) = table.get("record").and_then(toml::Value::as_table) {
        if let Some(v) = record.get("max_file_size") {
            cfg.record.max_file_size = value_to_bytes(v)?;
        }
        if let Some(v) = record.get("record_input").and_then(toml::Value::as_bool) {
            cfg.record.record_input = v;
        }
        if let Some(v) = record.get("record_binary").and_then(toml::Value::as_bool) {
            cfg.record.record_binary = v;
        }
        if let Some(v) = record.get("ignore").and_then(toml::Value::as_array) {
            cfg.record.ignore = v
                .iter()
                .filter_map(toml::Value::as_str)
                .map(String::from)
                .collect();
        }
    }
    if let Some(storage) = table.get("storage").and_then(toml::Value::as_table) {
        if let Some(v) = storage.get("data_dir").and_then(toml::Value::as_str) {
            cfg.storage.data_dir = PathBuf::from(v);
        }
    }
    if let Some(replay) = table.get("replay").and_then(toml::Value::as_table) {
        if let Some(v) = replay.get("theme").and_then(toml::Value::as_str) {
            cfg.replay.theme = match v {
                "auto" => Theme::Auto,
                "dark" => Theme::Dark,
                "light" => Theme::Light,
                other => {
                    return Err(ConfigError::Value(format!(
                        "replay.theme `{other}` not one of auto|dark|light"
                    ))
                    .into())
                }
            };
        }
    }
    if let Some(redaction) = table.get("redaction").and_then(toml::Value::as_table) {
        if let Some(v) = redaction.get("at_record").and_then(toml::Value::as_bool) {
            cfg.redaction.at_record = v;
        }
        if let Some(v) = redaction.get("entropy").and_then(toml::Value::as_bool) {
            cfg.redaction.entropy = v;
        }
        if let Some(v) = redaction.get("rules") {
            cfg.redaction.rules = parse_redaction_rules(v)?;
        }
    }
    Ok(())
}

/// Parse `[redaction] rules` — an array of `{ name = "...", pattern = "..." }`
/// tables. A malformed entry is an actionable error (silently dropping a
/// user's detector would be a redaction hole, the opposite of "warn, never
/// fail" — the *key* is known, its *value* is invalid).
fn parse_redaction_rules(v: &toml::Value) -> Result<Vec<RedactionRule>> {
    let Some(arr) = v.as_array() else {
        return Err(ConfigError::Value(
            "redaction.rules must be an array of { name, pattern } tables, e.g. \
             rules = [{ name = \"acme\", pattern = \"ACME-[0-9A-F]{16}\" }]"
                .into(),
        )
        .into());
    };
    let mut rules = Vec::with_capacity(arr.len());
    for (i, entry) in arr.iter().enumerate() {
        let table = entry.as_table().ok_or_else(|| {
            ConfigError::Value(format!(
                "redaction.rules[{i}] must be a table with string `name` and `pattern` keys"
            ))
        })?;
        let name = table
            .get("name")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                ConfigError::Value(format!("redaction.rules[{i}] is missing a string `name`"))
            })?;
        let pattern = table
            .get("pattern")
            .and_then(toml::Value::as_str)
            .ok_or_else(|| {
                ConfigError::Value(format!(
                    "redaction.rules[{i}] (`{name}`) is missing a string `pattern`"
                ))
            })?;
        rules.push(RedactionRule {
            name: name.to_string(),
            pattern: pattern.to_string(),
        });
    }
    Ok(rules)
}

fn value_to_bytes(v: &toml::Value) -> Result<u64> {
    match v {
        toml::Value::Integer(n) => u64::try_from(*n).map_err(|_| {
            ConfigError::Value(format!("max_file_size cannot be negative: {n}")).into()
        }),
        toml::Value::String(s) => Ok(parse_bytes(s).map_err(ConfigError::Value)?),
        other => Err(ConfigError::Value(format!(
            "max_file_size must be a string or integer, got {}",
            other.type_str()
        ))
        .into()),
    }
}

/// Resolved on-disk locations for the Halfhand data directory.
///
/// `#[non_exhaustive]`: already only ever built via [`Paths::resolve`] /
/// [`Paths::with_data_dir`], never a struct literal; this just makes that the
/// enforced contract so a future resolved path is additive.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Paths {
    /// The data directory itself (`$XDG_DATA_HOME/halfhand` by default).
    pub data_dir: PathBuf,
    /// The config file (`$XDG_CONFIG_HOME/halfhand/config.toml`).
    pub config_path: PathBuf,
    /// The SQLite database file (`<data_dir>/hh.db`).
    pub db_path: PathBuf,
    /// The blob store root (`<data_dir>/blobs`).
    pub blobs_dir: PathBuf,
}

impl Paths {
    /// Resolve paths from a loaded [`Config`] and the process environment.
    ///
    /// Precedence (SRS §2.3): `HH_DATA_DIR` env > `[storage] data_dir` file >
    /// platform default. The config file location is always the platform
    /// default (not overridable via config, only via `XDG_CONFIG_HOME`).
    pub fn resolve(config: &Config) -> Result<Self> {
        let env_dir = std::env::var_os("HH_DATA_DIR").filter(|s| !s.is_empty());
        let data_dir = if let Some(d) = env_dir {
            PathBuf::from(d)
        } else if !config.storage.data_dir.as_os_str().is_empty() {
            config.storage.data_dir.clone()
        } else {
            platform_data_dir()?
        };
        Ok(Self {
            db_path: data_dir.join("hh.db"),
            blobs_dir: data_dir.join("blobs"),
            config_path: platform_config_path()?,
            data_dir,
        })
    }

    /// Construct paths with an explicit data directory (used by tests so they
    /// never touch the real data directory, per CLAUDE.md testing standards).
    pub fn with_data_dir(data_dir: PathBuf) -> Self {
        Self {
            db_path: data_dir.join("hh.db"),
            blobs_dir: data_dir.join("blobs"),
            config_path: data_dir.join("config.toml"),
            data_dir,
        }
    }
}

fn platform_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", "halfhand").ok_or_else(|| {
        ConfigError::Value("cannot determine platform config/data directories (no HOME?)".into())
            .into()
    })
}

fn platform_data_dir() -> Result<PathBuf> {
    Ok(platform_dirs()?.data_dir().to_path_buf())
}

fn platform_config_path() -> Result<PathBuf> {
    Ok(platform_dirs()?.config_dir().join("config.toml"))
}

impl fmt::Display for Theme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Theme::Auto => "auto",
            Theme::Dark => "dark",
            Theme::Light => "light",
        };
        f.write_str(s)
    }
}

/// Fuzz-only entry point into the config.toml parser (`cargo fuzz` target
/// `config_toml`). Gated behind the `fuzzing` feature so it never widens the
/// crate's normal public API.
#[cfg(feature = "fuzzing")]
pub mod fuzzing {
    use super::{merge_table, warn_unknown_keys, Config};

    /// Mirrors [`Config::load`]'s parse+merge logic on arbitrary TOML text
    /// (skipping the file-read step, which is plain `std::fs::read_to_string`
    /// with no parsing of its own). Must never panic — only ever `Ok`/`Err`.
    pub fn fuzz_parse(s: &str) {
        let Ok(table) = toml::from_str::<toml::Table>(s) else {
            return;
        };
        warn_unknown_keys(&table);
        let mut cfg = Config::default();
        let _ = merge_table(&mut cfg, &table);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("config.toml");
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn parse_bytes_accepts_suffixes() {
        assert_eq!(parse_bytes("4MiB").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_bytes("512KiB").unwrap(), 512 * 1024);
        assert_eq!(parse_bytes("100B").unwrap(), 100);
        assert_eq!(parse_bytes("2048").unwrap(), 2048);
        assert!(parse_bytes("nope").is_err());
        assert!(parse_bytes("4PiB").is_err());
    }

    #[test]
    fn config_defaults() {
        let cfg = Config::default();
        assert_eq!(cfg.record.max_file_size, 4 * 1024 * 1024);
        assert!(!cfg.record.record_input);
        assert!(!cfg.record.record_binary);
        assert!(cfg.record.ignore.is_empty());
        assert_eq!(cfg.replay.theme, Theme::Auto);
        assert!(cfg.storage.data_dir.as_os_str().is_empty());
    }

    #[test]
    fn config_loads_known_keys() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            "\
[record]
max_file_size = \"1MiB\"
record_input = true
ignore = [\"dist/\", \"*.lock\"]

[storage]
data_dir = \"/tmp/hh-from-file\"

[replay]
theme = \"dark\"
",
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.record.max_file_size, 1024 * 1024);
        assert!(cfg.record.record_input);
        assert_eq!(cfg.record.ignore, vec!["dist/", "*.lock"]);
        assert_eq!(cfg.storage.data_dir, PathBuf::from("/tmp/hh-from-file"));
        assert_eq!(cfg.replay.theme, Theme::Dark);
    }

    #[test]
    fn config_unknown_keys_warn_but_load() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            "\
[record]
max_file_size = \"2MiB\"
mystery = true

[storage]
data_dir = \"/tmp/hh-unknown\"

[experimental]
feature = \"x\"
",
        );
        let cfg = Config::load(&path).unwrap();
        // Known keys still applied.
        assert_eq!(cfg.record.max_file_size, 2 * 1024 * 1024);
        assert_eq!(cfg.storage.data_dir, PathBuf::from("/tmp/hh-unknown"));
        // Unknown keys were tolerated (no panic, no error).
    }

    #[test]
    fn config_loads_redaction_section() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(
            tmp.path(),
            "\
[redaction]
at_record = true
entropy = false
rules = [{ name = \"acme\", pattern = \"ACME-[0-9A-F]{16}\" }]
",
        );
        let cfg = Config::load(&path).unwrap();
        assert!(cfg.redaction.at_record);
        assert!(!cfg.redaction.entropy);
        assert_eq!(
            cfg.redaction.rules,
            vec![RedactionRule {
                name: "acme".into(),
                pattern: "ACME-[0-9A-F]{16}".into(),
            }]
        );
        // Defaults: off, entropy on, no rules.
        let d = RedactionConfig::default();
        assert!(!d.at_record);
        assert!(d.entropy);
        assert!(d.rules.is_empty());
    }

    #[test]
    fn config_malformed_redaction_rules_error_actionably() {
        let tmp = TempDir::new().unwrap();
        // A rules entry missing `pattern` must be an error (a silently dropped
        // detector is a redaction hole), naming the rule.
        let path = write_config(tmp.path(), "[redaction]\nrules = [{ name = \"acme\" }]\n");
        let err = Config::load(&path).unwrap_err().to_string();
        assert!(err.contains("acme"), "must name the rule: {err}");
        // Non-array rules value.
        let path2 = write_config(tmp.path(), "[redaction]\nrules = \"nope\"\n");
        let err2 = Config::load(&path2).unwrap_err().to_string();
        assert!(err2.contains("array"), "must explain the shape: {err2}");
    }

    #[test]
    fn config_missing_file_is_default() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.toml");
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn config_malformed_toml_errors() {
        let tmp = TempDir::new().unwrap();
        let path = write_config(tmp.path(), "this is = = not toml");
        assert!(Config::load(&path).is_err());
    }

    #[test]
    fn paths_precedence_default_file_env() {
        // default: no file override, no env.
        std::env::remove_var("HH_DATA_DIR");
        let cfg = Config::default();
        let default_paths = Paths::resolve(&cfg).unwrap();
        assert!(default_paths.data_dir.ends_with("halfhand"));

        // file: storage.data_dir set in config.
        let cfg_file = Config {
            storage: StorageConfig {
                data_dir: PathBuf::from("/tmp/hh-file-wins"),
            },
            ..Config::default()
        };
        let file_paths = Paths::resolve(&cfg_file).unwrap();
        assert_eq!(file_paths.data_dir, PathBuf::from("/tmp/hh-file-wins"));

        // env overrides file.
        std::env::set_var("HH_DATA_DIR", "/tmp/hh-env-wins");
        let env_paths = Paths::resolve(&cfg_file).unwrap();
        assert_eq!(env_paths.data_dir, PathBuf::from("/tmp/hh-env-wins"));
        std::env::remove_var("HH_DATA_DIR");
    }

    #[test]
    fn paths_components_are_under_data_dir() {
        let p = Paths::with_data_dir(PathBuf::from("/tmp/hh-test"));
        assert_eq!(p.db_path, PathBuf::from("/tmp/hh-test/hh.db"));
        assert_eq!(p.blobs_dir, PathBuf::from("/tmp/hh-test/blobs"));
    }

    #[test]
    fn warn_on_ignored_halfhand_toml() {
        // When the canonical config.toml is present, a sibling halfhand.toml is
        // genuinely ignored → reported as a non-canonical file.
        let tmp = TempDir::new().unwrap();
        let canonical = tmp.path().join("config.toml");
        std::fs::write(&canonical, "[record]\nignore = [\"canonical\"]\n").unwrap();
        std::fs::write(
            tmp.path().join("halfhand.toml"),
            "[record]\nignore = [\"x\"]\n",
        )
        .unwrap();
        let ignored = ignored_noncanonical_config_files(&canonical);
        assert_eq!(ignored, vec![tmp.path().join("halfhand.toml")]);
        // No panic; the warning goes to stderr (not asserted here — behavior is
        // covered by an integration assertion on captured stderr elsewhere).
        warn_on_ignored_config_files(&canonical);

        // When the canonical config.toml is ABSENT, halfhand.toml is loaded as a
        // fallback (see Config::load) — it is NOT ignored, so neither function
        // reports it.
        let tmp2 = TempDir::new().unwrap();
        let canonical_absent = tmp2.path().join("config.toml");
        std::fs::write(
            tmp2.path().join("halfhand.toml"),
            "[record]\nignore = [\"y\"]\n",
        )
        .unwrap();
        assert!(!canonical_absent.exists());
        assert!(ignored_noncanonical_config_files(&canonical_absent).is_empty());
        warn_on_ignored_config_files(&canonical_absent);

        // Missing canonical parent dir: still no panic.
        warn_on_ignored_config_files(Path::new("/no/such/dir/config.toml"));
    }

    #[test]
    fn config_load_falls_back_to_halfhand_toml() {
        // config.toml absent + halfhand.toml present → halfhand.toml is loaded
        // (not ignored). The [storage] data_dir value takes effect.
        let tmp = TempDir::new().unwrap();
        let canonical = tmp.path().join("config.toml");
        std::fs::write(
            tmp.path().join("halfhand.toml"),
            "[storage]\ndata_dir = \"/tmp/hh-from-legacy\"\n",
        )
        .unwrap();
        assert!(!canonical.exists());
        let cfg = Config::load(&canonical).unwrap();
        assert_eq!(cfg.storage.data_dir, PathBuf::from("/tmp/hh-from-legacy"));
    }

    #[test]
    fn config_load_canonical_wins_over_halfhand_toml() {
        // Both present → canonical config.toml is read; halfhand.toml ignored.
        let tmp = TempDir::new().unwrap();
        let canonical = write_config(tmp.path(), "[storage]\ndata_dir = \"/tmp/hh-canonical\"\n");
        std::fs::write(
            tmp.path().join("halfhand.toml"),
            "[storage]\ndata_dir = \"/tmp/hh-legacy\"\n",
        )
        .unwrap();
        let cfg = Config::load(&canonical).unwrap();
        assert_eq!(cfg.storage.data_dir, PathBuf::from("/tmp/hh-canonical"));
    }
}
