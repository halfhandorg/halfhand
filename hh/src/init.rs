//! `hh init`: an explicit, explained first-time setup.
//!
//! Creates the data directory, blob store, exports directory and database
//! (running migrations), and writes a commented starter `config.toml` when no
//! config file exists. It prints every path it created or found. It is never
//! a prerequisite: every other command still creates what it needs on first
//! use (`open_store` in `main.rs`), so `hh init` only makes that visible and
//! adds the starter config.
//!
//! Idempotent: a second run creates nothing and says so. An existing config
//! is only replaced with `--force`, which saves it to `config.toml.bak`
//! first. `--migrate-config` renames a legacy `halfhand.toml` / `hh.toml` to
//! `config.toml`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use hh_core::config::{legacy_fallback_path, Config, ConfigSource, Paths};
use hh_core::store::Store;
use owo_colors::OwoColorize;

use crate::{cli, render};

/// Name of the backup `--force` writes next to `config.toml`.
const BACKUP_NAME: &str = "config.toml.bak";

/// What happened to one artefact during `hh init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    /// It did not exist and was created now.
    Created,
    /// It already existed and was left untouched.
    Found,
    /// The config was rewritten from the template (`--force`).
    Replaced,
    /// A legacy config file was renamed to `config.toml`.
    Renamed,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Created => "created",
            Status::Found => "found",
            Status::Replaced => "replaced",
            Status::Renamed => "renamed",
        }
    }
}

/// One labelled row of the `hh init` report.
#[derive(Debug, Clone)]
pub(crate) struct Artefact {
    /// Fixed label: `config`, `data dir`, `database`, `blobs`, `exports`.
    pub label: &'static str,
    /// Absolute path.
    pub path: PathBuf,
    /// What `hh init` did to it.
    pub status: Status,
}

/// Everything `hh init` did, ready to render. Pure data so the output can be
/// snapshot-tested without touching the filesystem.
#[derive(Debug, Clone)]
pub(crate) struct InitReport {
    /// The five artefacts, in display order.
    pub artefacts: Vec<Artefact>,
    /// Extra lines explaining something the user should know (legacy config
    /// in use, a backup written, a shared config/data directory...).
    pub notes: Vec<String>,
    /// Whether the data directory is created private (mode 0700): Unix only.
    pub private_mode: bool,
}

impl InitReport {
    /// `true` when this run changed nothing on disk.
    fn unchanged(&self) -> bool {
        self.artefacts.iter().all(|a| a.status == Status::Found)
    }
}

/// `hh init`.
pub(crate) fn init_command(args: &cli::InitArgs) -> anyhow::Result<ExitCode> {
    let report = run_init(args)?;
    print!("{}", render_plain(&report, render::use_color()));
    Ok(ExitCode::SUCCESS)
}

/// Do the work and describe it.
fn run_init(args: &cli::InitArgs) -> anyhow::Result<InitReport> {
    let canonical = Paths::resolve(&Config::default())
        .map_err(|e| {
            anyhow::anyhow!(
                "could not resolve the config directory\n  why: {e}\n  \
                 hint: make sure HOME (USERPROFILE on Windows) is set"
            )
        })?
        .config_path;
    let config_dir = canonical
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let mut notes = Vec::new();

    let config_row = setup_config(args, &canonical, &config_dir, &mut notes)?;

    // Load what will actually be read (after any rename/rewrite above) so
    // `[storage] data_dir` is honored. A broken config is an error here: init
    // must not paper over it with defaults and report a misleading data dir.
    let (config, source) = Config::load_with_source(&canonical).map_err(|e| {
        anyhow::anyhow!(
            "could not load {}\n  why: {e}\n  \
             hint: fix the file, or run `hh init --force` to replace it with the \
             starter template (the old file is kept as {BACKUP_NAME})",
            config_row.path.display()
        )
    })?;
    for ignored in hh_core::config::ignored_noncanonical_config_files(&canonical) {
        notes.push(format!(
            "{} is ignored because config.toml exists; move its settings into config.toml",
            display(&ignored)
        ));
    }
    let paths = Paths::resolve(&config).map_err(|e| {
        anyhow::anyhow!(
            "could not resolve the data directory\n  why: {e}\n  \
             hint: set HH_DATA_DIR to a writable directory"
        )
    })?;
    let data_dir = absolute(&paths.data_dir);
    let exports = data_dir.join("exports");

    let [data_status, db_status, blobs_status, exports_status] =
        create_data(&paths, &data_dir, &exports)?;

    if same_dir(&config_dir, &data_dir) {
        notes.push(format!(
            "config and data share one directory ({}). Deleting it to clear \
             sessions also deletes your config; use `hh delete` or `hh gc` instead.",
            display(&data_dir)
        ));
    }
    if let ConfigSource::Legacy(legacy) = &source {
        notes.push(format!(
            "reading {} (config.toml does not exist). Run `hh init --migrate-config` \
             to rename it to config.toml.",
            display(legacy)
        ));
    }

    Ok(InitReport {
        artefacts: vec![
            config_row,
            Artefact {
                label: "data dir",
                path: data_dir.clone(),
                status: data_status,
            },
            Artefact {
                label: "database",
                path: absolute(&paths.db_path),
                status: db_status,
            },
            Artefact {
                label: "blobs",
                path: absolute(&paths.blobs_dir),
                status: blobs_status,
            },
            Artefact {
                label: "exports",
                path: exports,
                status: exports_status,
            },
        ],
        notes,
        private_mode: cfg!(unix),
    })
}

/// Create the data dir, blob store, database (running migrations) and
/// exports directory. Returns each one's status, sampled before creation.
fn create_data(paths: &Paths, data_dir: &Path, exports: &Path) -> anyhow::Result<[Status; 4]> {
    let before = |p: &Path| {
        if p.exists() {
            Status::Found
        } else {
            Status::Created
        }
    };
    let data_status = before(data_dir);
    let db_status = before(&paths.db_path);
    let blobs_status = before(&paths.blobs_dir);
    let exports_status = before(exports);

    // Store::open creates the data dir and blob store (0700 on Unix) and
    // runs migrations idempotently.
    let store = Store::open(&paths.db_path, &paths.blobs_dir).map_err(|e| {
        anyhow::anyhow!(
            "could not create the database at {}\n  why: {e}\n  \
             hint: check permissions on {}, or set HH_DATA_DIR to a writable directory",
            paths.db_path.display(),
            data_dir.display()
        )
    })?;
    drop(store);
    create_private_dir(exports).map_err(|e| {
        anyhow::anyhow!(
            "could not create the exports directory {}\n  why: {e}\n  \
             hint: check permissions on {}",
            exports.display(),
            data_dir.display()
        )
    })?;

    Ok([data_status, db_status, blobs_status, exports_status])
}

/// The config half of `hh init`: rename, rewrite or create `config.toml`, or
/// report the file already in use. Returns the `config` row.
fn setup_config(
    args: &cli::InitArgs,
    canonical: &Path,
    config_dir: &Path,
    notes: &mut Vec<String>,
) -> anyhow::Result<Artefact> {
    let row = |path: &Path, status| Artefact {
        label: "config",
        path: absolute(path),
        status,
    };
    let mut legacy = if canonical.exists() {
        None
    } else {
        legacy_fallback_path(canonical)
    };

    let mut renamed = None;
    if args.migrate_config {
        match &legacy {
            Some(from) => {
                std::fs::rename(from, canonical).map_err(|e| {
                    anyhow::anyhow!(
                        "could not rename {} to {}\n  why: {e}\n  \
                         hint: rename it by hand, or check permissions on {}",
                        from.display(),
                        canonical.display(),
                        config_dir.display()
                    )
                })?;
                notes.push(format!(
                    "renamed {} to {}",
                    display(from),
                    display(canonical)
                ));
                renamed = Some(row(canonical, Status::Renamed));
                legacy = None;
            }
            None if canonical.exists() => notes.push(
                "--migrate-config: config.toml already exists; nothing to rename".to_string(),
            ),
            None => notes.push(
                "--migrate-config: no halfhand.toml or hh.toml found; nothing to rename"
                    .to_string(),
            ),
        }
    }

    if args.force {
        if let Some(legacy) = &legacy {
            anyhow::bail!(
                "refusing to write config.toml next to {}\n  \
                 why: Halfhand is reading that file; a new config.toml would \
                 silently take its place\n  \
                 hint: run `hh init --migrate-config --force` to rename it first \
                 (it is then backed up to {BACKUP_NAME})",
                legacy.display()
            );
        }
        if canonical.exists() {
            let backup = canonical.with_file_name(BACKUP_NAME);
            std::fs::copy(canonical, &backup).map_err(|e| {
                anyhow::anyhow!(
                    "could not back up {} to {}\n  why: {e}\n  \
                     hint: nothing was changed; check permissions on {}",
                    canonical.display(),
                    backup.display(),
                    config_dir.display()
                )
            })?;
            write_template(canonical, config_dir)?;
            notes.push(format!("previous config saved to {}", display(&backup)));
            return Ok(row(canonical, Status::Replaced));
        }
    }

    if let Some(done) = renamed {
        return Ok(done);
    }
    if canonical.exists() {
        return Ok(row(canonical, Status::Found));
    }
    if let Some(legacy) = legacy {
        return Ok(row(&legacy, Status::Found));
    }
    write_template(canonical, config_dir)?;
    Ok(row(canonical, Status::Created))
}

/// Write the starter template to `path`, creating `config_dir` (0700 on
/// Unix: on macOS it is also the data directory).
fn write_template(path: &Path, config_dir: &Path) -> anyhow::Result<()> {
    create_private_dir(config_dir).map_err(|e| {
        anyhow::anyhow!(
            "could not create the config directory {}\n  why: {e}\n  \
             hint: check permissions on its parent directory",
            config_dir.display()
        )
    })?;
    std::fs::write(path, hh_core::starter_config_template()).map_err(|e| {
        anyhow::anyhow!(
            "could not write {}\n  why: {e}\n  hint: check permissions on {}",
            path.display(),
            config_dir.display()
        )
    })
}

/// Create `path` and any missing parents, private (0700) on Unix. Existing
/// directories keep their mode.
fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

/// `path` made absolute against the current directory (no symlink
/// resolution); unchanged if that fails.
fn absolute(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Whether two directories are the same place, comparing canonical paths
/// when both exist.
fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => absolute(a) == absolute(b),
    }
}

fn display(path: &Path) -> String {
    absolute(path).display().to_string()
}

/// Render the report: a headline, aligned `label  status  path` rows, notes,
/// and a next step. One accent color (green) on the glyph only.
pub(crate) fn render_plain(report: &InitReport, color: bool) -> String {
    let mut out = String::new();
    let check = if color {
        "✓".green().to_string()
    } else {
        "✓".to_string()
    };
    let headline = if report.unchanged() {
        "Halfhand is already set up; nothing changed"
    } else {
        "Halfhand is set up"
    };
    let _ = writeln!(out, "{check} {headline}\n");
    let label_w = report
        .artefacts
        .iter()
        .map(|a| a.label.len())
        .max()
        .unwrap_or(0);
    let status_w = report
        .artefacts
        .iter()
        .map(|a| a.status.label().len())
        .max()
        .unwrap_or(0);
    for a in &report.artefacts {
        let _ = writeln!(
            out,
            "  {label:<label_w$}  {status:<status_w$}  {path}",
            label = a.label,
            status = a.status.label(),
            path = a.path.display()
        );
    }
    out.push('\n');
    for note in &report.notes {
        let _ = writeln!(out, "● {note}");
    }
    if report.private_mode {
        let _ = writeln!(
            out,
            "● Sessions hold prompts and source code, so new directories are private (mode 0700)."
        );
    }
    let _ = writeln!(out, "\nNext: hh run -- <agent>   e.g. hh run -- claude");
    out
}

/// Init status for `hh doctor`: which artefacts `hh init` would create.
/// Computed before `open_store` (which creates the data dir, database and
/// blobs implicitly) so it reflects the state the user left.
pub(crate) struct InitStatus {
    /// `(label, absolute path)` of each artefact that does not exist yet.
    pub missing: Vec<(&'static str, PathBuf)>,
}

impl InitStatus {
    /// Probe the filesystem; read-only.
    pub(crate) fn probe() -> anyhow::Result<Self> {
        let canonical = Paths::resolve(&Config::default())?.config_path;
        let config = Config::load(&canonical).unwrap_or_default();
        let paths = Paths::resolve(&config)?;
        let mut missing = Vec::new();
        if !canonical.exists() && legacy_fallback_path(&canonical).is_none() {
            missing.push(("config", absolute(&canonical)));
        }
        let data_dir = absolute(&paths.data_dir);
        for (label, path) in [
            ("data dir", data_dir.clone()),
            ("database", absolute(&paths.db_path)),
            ("blobs", absolute(&paths.blobs_dir)),
            ("exports", data_dir.join("exports")),
        ] {
            if !path.exists() {
                missing.push((label, path));
            }
        }
        Ok(Self { missing })
    }

    /// `true` when every artefact `hh init` creates is present.
    pub(crate) fn initialized(&self) -> bool {
        self.missing.is_empty()
    }

    /// The additive `init` object in `hh doctor --json`.
    pub(crate) fn to_json(&self) -> serde_json::Value {
        let would_create: Vec<serde_json::Value> = self
            .missing
            .iter()
            .map(|(label, path)| serde_json::json!({ "label": label, "path": path.display().to_string() }))
            .collect();
        serde_json::json!({
            "initialized": self.initialized(),
            "would_create": would_create,
        })
    }

    /// The informational `● init — ...` line in plain `hh doctor` output.
    pub(crate) fn render_plain(&self) -> String {
        if self.initialized() {
            return "● init — done; config, data dir, database, blobs and exports all exist\n"
                .to_string();
        }
        let list: Vec<String> = self
            .missing
            .iter()
            .map(|(label, path)| format!("{label} {}", path.display()))
            .collect();
        format!(
            "● init — not run; `hh init` would create: {}\n",
            list.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(status: Status, notes: Vec<String>) -> InitReport {
        let row = |label, path: &str, s| Artefact {
            label,
            path: PathBuf::from(path),
            status: s,
        };
        InitReport {
            artefacts: vec![
                row("config", "/home/u/.config/halfhand/config.toml", status),
                row("data dir", "/home/u/.local/share/halfhand", status),
                row("database", "/home/u/.local/share/halfhand/hh.db", status),
                row("blobs", "/home/u/.local/share/halfhand/blobs", status),
                row("exports", "/home/u/.local/share/halfhand/exports", status),
            ],
            notes,
            private_mode: true,
        }
    }

    #[test]
    fn snapshot_init_first_run() {
        insta::assert_snapshot!(render_plain(&report(Status::Created, vec![]), false));
    }

    #[test]
    fn snapshot_init_second_run_says_nothing_changed() {
        let out = render_plain(&report(Status::Found, vec![]), false);
        assert!(out.contains("nothing changed"), "{out}");
        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_init_force_and_shared_dir_notes() {
        let mut r = report(
            Status::Found,
            vec![
                "config and data share one directory (/home/u/.local/share/halfhand). \
                 Deleting it to clear sessions also deletes your config; use `hh delete` or `hh gc` instead."
                    .to_string(),
            ],
        );
        r.artefacts[0].status = Status::Replaced;
        insta::assert_snapshot!(render_plain(&r, false));
    }

    #[test]
    fn doctor_init_status_lists_missing_artefacts() {
        let s = InitStatus {
            missing: vec![("exports", PathBuf::from("/d/exports"))],
        };
        assert!(!s.initialized());
        assert_eq!(
            s.to_json(),
            serde_json::json!({
                "initialized": false,
                "would_create": [{ "label": "exports", "path": "/d/exports" }],
            })
        );
        assert_eq!(
            s.render_plain(),
            "● init — not run; `hh init` would create: exports /d/exports\n"
        );
        let done = InitStatus { missing: vec![] };
        assert!(done.initialized());
        assert!(done.render_plain().contains("done"));
    }
}
