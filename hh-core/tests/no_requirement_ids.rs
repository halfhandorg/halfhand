//! Guard: requirement and ticket IDs (`SRS`, `FR-n`, `NFR-n`, `DR-n`,
//! `BUG-n`, `ENH-n`, `AC-n`, `Area n`) must not appear in the workspace's code,
//! comments, tests or docs. Behaviour is described in plain words instead.
//!
//! Allowed: the historical requirements documents (`halfhand-srs-*.md`),
//! insta snapshots, `CLAUDE.md` (which states the rule and so names the
//! tokens), the help-text guard test in `hh/src/cli.rs` (whose sample strings
//! are forbidden tokens on purpose), and this file.

use std::fs;
use std::path::{Path, PathBuf};

const EXTENSIONS: &[&str] = &["rs", "md", "toml", "yml", "yaml", "sql", "sh", "py", "txt"];
const SKIP_DIRS: &[&str] = &["target", ".git", "node_modules"];
const ALLOWED_FILES: &[&str] = &[
    "CLAUDE.md",
    "hh/src/cli.rs",
    "hh-core/tests/no_requirement_ids.rs",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("hh-core sits inside the workspace root")
        .to_path_buf()
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if !SKIP_DIRS.contains(&name.as_str()) {
                collect(&path, out);
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.contains(&e))
            && !name.starts_with("halfhand-srs-")
        {
            out.push(path);
        }
    }
}

/// True if `word` (already stripped of surrounding punctuation) is an ID
/// token: `SRS`, or `<PREFIX>-<digit>…` for one of the known prefixes.
fn is_id_token(word: &str) -> bool {
    if word == "SRS" {
        return true;
    }
    ["FR-", "NFR-", "DR-", "BUG-", "ENH-", "AC-"]
        .iter()
        .any(|p| {
            word.strip_prefix(p)
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
}

/// IDs found on `line`, including the two-word `Area <n>` form.
fn ids_in(line: &str) -> Vec<String> {
    let words: Vec<&str> = line
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '.'))
        .map(|w| w.trim_matches(|c: char| c == '.' || c == '-'))
        .filter(|w| !w.is_empty())
        .collect();
    let mut found = Vec::new();
    for (i, w) in words.iter().enumerate() {
        if is_id_token(w) {
            found.push((*w).to_string());
        }
        if *w == "Area"
            && words
                .get(i + 1)
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
        {
            found.push(format!("Area {}", words[i + 1]));
        }
    }
    found
}

#[test]
fn id_detector_matches_the_forbidden_forms() {
    assert_eq!(ids_in("Record a session (SRS FR-1.1)."), ["SRS", "FR-1.1"]);
    assert_eq!(
        ids_in("see NFR-2, DR-4 and BUG-7"),
        ["NFR-2", "DR-4", "BUG-7"]
    );
    assert_eq!(
        ids_in("fixes ENH-12 / AC-5 (Area 3)"),
        ["ENH-12", "AC-5", "Area 3"]
    );
    assert_eq!(
        ids_in("FROM sessions; the BUGS list; a FR- prefix; Area 51b"),
        Vec::<String>::new()
    );
    assert_eq!(
        ids_in("RUSTSEC-2026-0002 is an advisory, not a ticket"),
        Vec::<String>::new()
    );
}

#[test]
fn no_requirement_ids_in_the_workspace() {
    let root = workspace_root();
    let mut files = Vec::new();
    collect(&root, &mut files);
    assert!(
        files.len() > 50,
        "walked only {} files from {}; is the workspace root right?",
        files.len(),
        root.display()
    );

    let mut hits = Vec::new();
    for path in files {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if ALLOWED_FILES.contains(&rel.as_str()) {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            for id in ids_in(line) {
                hits.push(format!("{rel}:{}: {id}", n + 1));
            }
        }
    }
    assert_eq!(
        hits,
        Vec::<String>::new(),
        "requirement/ticket IDs found; describe the behaviour in plain words instead \
         (see CLAUDE.md)"
    );
}
