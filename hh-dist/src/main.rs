//! Release-asset generator for Halfhand.
//!
//! Emits `hh` shell completion scripts and the `hh.1` man page into an output
//! directory, reusing [`halfhand::cli::Cli`] as the single source of truth so
//! generated assets never drift from the real `hh` binary.
//!
//! This is a build-host tool (run on the release runner, not a cross target):
//! completion scripts and man pages are architecture-independent, so one
//! generated set is bundled into every per-target archive. Invoked by
//! cargo-dist's `extra-artifacts` step and by `just dist-assets`:
//!
//! ```text
//! cargo run -p hh-dist -- <out-dir>
//! ```
//!
//! Writes:
//! - `<out-dir>/completions/hh.bash`, `_hh` (zsh), `hh.fish`, `_hh.ps1`
//! - `<out-dir>/man/hh.1`
//!
//! Errors only (no panics) on missing args / IO failure.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::CommandFactory;
use clap_complete::Shell;
use clap_mangen::Man;

use halfhand::cli::Cli;

fn main() -> ExitCode {
    let out = match parse_out_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hh-gen-assets: {e}");
            eprintln!("usage: hh-gen-assets <out-dir>");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = run(&out) {
        eprintln!("hh-gen-assets: {e:#}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}

/// Parse the single positional `<out-dir>` argument.
fn parse_out_dir() -> Result<PathBuf> {
    let mut args = std::env::args().skip(1);
    let out = args.next().context("missing <out-dir> argument")?;
    if args.next().is_some() {
        anyhow::bail!("expected exactly one argument: <out-dir>");
    }
    Ok(PathBuf::from(out))
}

/// Generate all assets into `out`.
fn run(out: &Path) -> Result<()> {
    let comp_dir = out.join("completions");
    let man_dir = out.join("man");
    fs::create_dir_all(&comp_dir).with_context(|| format!("creating {}", comp_dir.display()))?;
    fs::create_dir_all(&man_dir).with_context(|| format!("creating {}", man_dir.display()))?;

    let mut cmd = Cli::command();
    let bin = cmd.get_name().to_owned();

    // One completion file per shell, named to match the docs' install
    // instructions and Homebrew's conventions.
    let shells = [
        (Shell::Bash, "hh.bash"),
        (Shell::Zsh, "_hh"),
        (Shell::Fish, "hh.fish"),
        (Shell::PowerShell, "_hh.ps1"),
    ];
    for (shell, name) in shells {
        let path = comp_dir.join(name);
        let mut file =
            fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
        clap_complete::generate(shell, &mut cmd, bin.clone(), &mut file);
        file.flush()
            .with_context(|| format!("flushing {}", path.display()))?;
    }

    // Man page `hh.1`.
    let man = Man::new(cmd.clone());
    let man_path = man_dir.join("hh.1");
    let mut man_file =
        fs::File::create(&man_path).with_context(|| format!("creating {}", man_path.display()))?;
    man.render(&mut man_file)
        .with_context(|| format!("rendering {}", man_path.display()))?;
    Ok(())
}
