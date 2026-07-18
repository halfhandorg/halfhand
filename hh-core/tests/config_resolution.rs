//! Integration tests for BUG-2 (SRS v1.1.0 §3): the three-step config
//! resolution order (`config.toml` → `halfhand.toml` → built-in defaults) and
//! the one-time, TTY-only, suppressible legacy-filename hint.
//!
//! These tests drive the public API only — `Config::load_with_source` and
//! `print_legacy_config_hint_if_needed_with` — against temp config/data dirs
//! so they never touch the real Halfhand data directory (CLAUDE.md testing
//! standards). The TTY decision is injected via the `_with` variant because a
//! test runner's stderr is typically piped (not a TTY), which would otherwise
//! make the TTY-gated hint path untestable.

#![allow(clippy::missing_docs_in_private_items)]

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use hh_core::config::{
    legacy_config_hint_marker_path, print_legacy_config_hint_if_needed_with, Config, ConfigSource,
    Paths,
};
use tempfile::TempDir;

/// `HH_NO_CONFIG_HINT` and `HH_DATA_DIR` are process-global env vars. Rust's
/// test harness runs tests in parallel threads within one process, so two
/// tests that touch the same env var race each other and flake. This mutex
/// serializes every test that reads or writes either var: a test takes the
/// guard before mutating the env and holds it until the end of its body, so
/// no two such tests ever observe each other's env state. `const`-constructible
/// since Rust 1.63, so fine at the 1.75 MSRV.
static ENV_GUARD: Mutex<()> = Mutex::new(());

/// Write `contents` to `dir/config.toml` and return its path.
fn write_canonical(dir: &std::path::Path, contents: &str) -> PathBuf {
    let p = dir.join("config.toml");
    fs::write(&p, contents).unwrap();
    p
}

/// Write `contents` to `dir/halfhand.toml` (the legacy name) and return its
/// path. Does not create `config.toml`.
fn write_legacy(dir: &std::path::Path, contents: &str) -> PathBuf {
    let p = dir.join("halfhand.toml");
    fs::write(&p, contents).unwrap();
    p
}

/// A sentinel `[storage] data_dir` value used to detect which file was loaded.
/// Using `data_dir` (not `ignore`) keeps the assertion independent of the
/// watcher path-filtering code.
const CANONICAL_MARKER: &str = "/tmp/hh-bug2-canonical";
const LEGACY_MARKER: &str = "/tmp/hh-bug2-legacy";

/// Test 1 (BUG-2.4): canonical present, legacy absent → canonical loaded, no
/// hint. `ConfigSource::Canonical` is returned and the hint helper declines to
/// print (returns `false`) without touching the marker.
#[test]
fn canonical_present_legacy_absent_loads_canonical_no_hint() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::remove_var("HH_NO_CONFIG_HINT");

    let cfg_dir = TempDir::new().unwrap();
    let canonical = write_canonical(
        cfg_dir.path(),
        &format!("[storage]\ndata_dir = \"{CANONICAL_MARKER}\"\n"),
    );
    let data_dir = TempDir::new().unwrap();
    let marker = legacy_config_hint_marker_path(data_dir.path());

    let (cfg, source) = Config::load_with_source(&canonical).unwrap();
    assert_eq!(source, ConfigSource::Canonical);
    assert_eq!(cfg.storage.data_dir, PathBuf::from(CANONICAL_MARKER));

    // The hint must not fire for a Canonical source, even with a TTY and no
    // marker and no env suppression — the gate is on the *source*, not just
    // the suppression mechanisms.
    let printed =
        print_legacy_config_hint_if_needed_with(&source, &canonical, data_dir.path(), true);
    assert!(!printed, "no hint when canonical is loaded");
    assert!(
        !marker.exists(),
        "no marker written when canonical is loaded"
    );
}

/// Test 2 (BUG-2.2): canonical absent, legacy present → legacy loaded, hint
/// printed once. `ConfigSource::Legacy` is returned, the hint fires on the
/// first call (with TTY + no env suppress + no marker), and the marker is
/// written as a side effect.
#[test]
fn canonical_absent_legacy_present_loads_legacy_prints_hint_once() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::remove_var("HH_NO_CONFIG_HINT");

    let cfg_dir = TempDir::new().unwrap();
    let legacy = write_legacy(
        cfg_dir.path(),
        &format!("[storage]\ndata_dir = \"{LEGACY_MARKER}\"\n"),
    );
    let canonical = cfg_dir.path().join("config.toml");
    assert!(!canonical.exists());

    let data_dir = TempDir::new().unwrap();
    let marker = legacy_config_hint_marker_path(data_dir.path());
    assert!(!marker.exists());

    let (cfg, source) = Config::load_with_source(&canonical).unwrap();
    assert_eq!(
        source,
        ConfigSource::Legacy(legacy.clone()),
        "legacy file must be reported as the source"
    );
    assert_eq!(cfg.storage.data_dir, PathBuf::from(LEGACY_MARKER));

    // First call with TTY + no env + no marker → prints and writes marker.
    let printed =
        print_legacy_config_hint_if_needed_with(&source, &canonical, data_dir.path(), true);
    assert!(printed, "hint must print on first legacy load with a TTY");
    assert!(
        marker.exists(),
        "marker must be written after the hint fires"
    );
}

/// Test 3 (BUG-2.1): both absent → defaults loaded, no hint. `ConfigSource::Defaults`
/// is returned and the hint helper declines regardless of TTY/marker state.
#[test]
fn both_absent_loads_defaults_no_hint() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::remove_var("HH_NO_CONFIG_HINT");

    let cfg_dir = TempDir::new().unwrap();
    let canonical = cfg_dir.path().join("config.toml");
    assert!(!canonical.exists());
    assert!(!cfg_dir.path().join("halfhand.toml").exists());

    let data_dir = TempDir::new().unwrap();
    let marker = legacy_config_hint_marker_path(data_dir.path());

    let (cfg, source) = Config::load_with_source(&canonical).unwrap();
    assert_eq!(source, ConfigSource::Defaults);
    assert_eq!(cfg, Config::default());

    let printed =
        print_legacy_config_hint_if_needed_with(&source, &canonical, data_dir.path(), true);
    assert!(!printed, "no hint when no config file was found");
    assert!(!marker.exists(), "no marker written when defaults are used");
}

/// Test 4 (BUG-2.1): both present → canonical wins, no hint. `ConfigSource::Canonical`
/// is returned (the legacy file is *ignored*, not loaded as a fallback), and
/// the hint does not fire.
#[test]
fn both_present_canonical_wins_no_hint() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::remove_var("HH_NO_CONFIG_HINT");

    let cfg_dir = TempDir::new().unwrap();
    let canonical = write_canonical(
        cfg_dir.path(),
        &format!("[storage]\ndata_dir = \"{CANONICAL_MARKER}\"\n"),
    );
    let _legacy = write_legacy(
        cfg_dir.path(),
        &format!("[storage]\ndata_dir = \"{LEGACY_MARKER}\"\n"),
    );

    let data_dir = TempDir::new().unwrap();
    let marker = legacy_config_hint_marker_path(data_dir.path());

    let (cfg, source) = Config::load_with_source(&canonical).unwrap();
    assert_eq!(source, ConfigSource::Canonical);
    // Canonical value wins; legacy value must NOT leak through.
    assert_eq!(cfg.storage.data_dir, PathBuf::from(CANONICAL_MARKER));
    assert_ne!(cfg.storage.data_dir, PathBuf::from(LEGACY_MARKER));

    let printed =
        print_legacy_config_hint_if_needed_with(&source, &canonical, data_dir.path(), true);
    assert!(!printed, "no hint when canonical is present (it wins)");
    assert!(!marker.exists(), "no marker written when canonical wins");
}

/// Test 5 (BUG-2.2): the persistent marker suppresses the hint on a second
/// run. First call writes the marker; second call (same data dir, still a
/// TTY, no env suppress) must NOT print again.
#[test]
fn hint_suppressed_on_second_run_by_marker() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::remove_var("HH_NO_CONFIG_HINT");

    let cfg_dir = TempDir::new().unwrap();
    let legacy = write_legacy(
        cfg_dir.path(),
        &format!("[storage]\ndata_dir = \"{LEGACY_MARKER}\"\n"),
    );
    let canonical = cfg_dir.path().join("config.toml");
    let data_dir = TempDir::new().unwrap();
    let marker = legacy_config_hint_marker_path(data_dir.path());

    let (_, source) = Config::load_with_source(&canonical).unwrap();
    assert_eq!(source, ConfigSource::Legacy(legacy));

    // First invocation: hint fires, marker written.
    assert!(print_legacy_config_hint_if_needed_with(
        &source,
        &canonical,
        data_dir.path(),
        true
    ));
    assert!(marker.exists(), "marker must exist after first invocation");

    // Second invocation: same source, same TTY, same (absent) env suppress —
    // the marker alone must now suppress the hint.
    assert!(
        !print_legacy_config_hint_if_needed_with(&source, &canonical, data_dir.path(), true),
        "hint must be suppressed on the second run by the persistent marker"
    );
}

/// Test 6 (BUG-2.2): `HH_NO_CONFIG_HINT=1` suppresses the hint even on the
/// first run with a TTY and no marker. The marker must NOT be written (the env
/// var is a full suppression, not a "pretend it was already shown").
#[test]
fn hint_suppressed_by_env_var() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::set_var("HH_NO_CONFIG_HINT", "1");

    let cfg_dir = TempDir::new().unwrap();
    let _legacy = write_legacy(
        cfg_dir.path(),
        &format!("[storage]\ndata_dir = \"{LEGACY_MARKER}\"\n"),
    );
    let canonical = cfg_dir.path().join("config.toml");
    let data_dir = TempDir::new().unwrap();
    let marker = legacy_config_hint_marker_path(data_dir.path());

    let (_, source) = Config::load_with_source(&canonical).unwrap();
    assert!(matches!(source, ConfigSource::Legacy(_)));

    // Env var set + TTY + no marker → still suppressed, and no marker written
    // (so a later run with the env var unset would still hint once).
    let printed =
        print_legacy_config_hint_if_needed_with(&source, &canonical, data_dir.path(), true);
    assert!(!printed, "HH_NO_CONFIG_HINT=1 must suppress the hint");
    assert!(
        !marker.exists(),
        "env-var suppression must not write the marker (it is a one-shot escape hatch, not a 'mark as shown')"
    );

    // Clean up the env var so it can't leak to other tests via the same
    // process. The ENV_GUARD mutex makes this safe even if another test was
    // about to start.
    std::env::remove_var("HH_NO_CONFIG_HINT");
}

/// Non-TTY stderr suppresses the hint even when everything else would fire
/// (legacy source, no env var, no marker). This is the SRS's "TTY only" gate:
/// piped/JSON output must never be corrupted by the hint. Not one of the six
/// required tests but exercises a path the SRS calls out explicitly.
#[test]
fn hint_suppressed_when_stderr_is_not_a_tty() {
    let _g = ENV_GUARD
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::env::remove_var("HH_NO_CONFIG_HINT");

    let cfg_dir = TempDir::new().unwrap();
    let _legacy = write_legacy(
        cfg_dir.path(),
        &format!("[storage]\ndata_dir = \"{LEGACY_MARKER}\"\n"),
    );
    let canonical = cfg_dir.path().join("config.toml");
    let data_dir = TempDir::new().unwrap();
    let marker = legacy_config_hint_marker_path(data_dir.path());

    let (_, source) = Config::load_with_source(&canonical).unwrap();
    assert!(matches!(source, ConfigSource::Legacy(_)));

    let printed = print_legacy_config_hint_if_needed_with(
        &source,
        &canonical,
        data_dir.path(),
        false, // non-TTY
    );
    assert!(!printed, "hint must not print when stderr is not a TTY");
    assert!(
        !marker.exists(),
        "no marker written when the hint was suppressed by non-TTY stderr"
    );
}

/// Sanity: the marker path helper agrees with the constant the binary uses.
/// `Paths::with_data_dir` is the test seam the rest of the suite uses to keep
/// tests out of the real data dir.
#[test]
fn marker_path_lives_under_data_dir() {
    let p = Paths::with_data_dir(PathBuf::from("/tmp/hh-bug2-marker"));
    assert_eq!(
        legacy_config_hint_marker_path(&p.data_dir),
        p.data_dir.join(".config-hint-shown")
    );
}
