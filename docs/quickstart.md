# Quickstart

## Install

Pick one. All three install the `hh` binary.

**cargo** (any OS with a Rust toolchain):

```bash
cargo install halfhand
```

**Homebrew** (macOS/Linux):

```bash
brew tap halfhand-org/tap
brew install hh
```

**Shell installer** (downloads a prebuilt binary + checksum from the latest
[GitHub release](https://github.com/halfhandorg/halfhand/releases)):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://halfhandorg.github.io/halfhand/install.sh | sh
```

> The installer places `hh` in `~/.halfhand/bin` by default; add it to your
> `PATH` if it isn't already.

Verify:

```bash
hh --version
```

## 30-second demo

```bash
hh run -- claude                        # record a Claude Code session (or any command)
hh replay last                          # faithfully play it back in an interactive TUI
hh inspect last                         # non-interactive summary + step table
hh list                                 # every recording, newest first
```

That's the whole loop. `hh delete last --yes` removes a recording.

## Where your data lives

Recordings go in your Halfhand data dir (`~/.local/share/halfhand/` on Linux,
`~/Library/Application Support/halfhand/` on macOS by default). Point it
elsewhere with `HH_DATA_DIR`:

```bash
HH_DATA_DIR=./hh-data hh run -- echo hello
HH_DATA_DIR=./hh-data hh list
```

## Next

- [Recording](./recording.md) — `hh run` flags and what gets captured.
- [Replay & Inspect](./replay-inspect.md) — the TUI and the JSON view.
- [Redaction](./redaction.md) — scan and remove secrets before sharing.
- [Export & import](./export-import.md) — portable `.hh` bundles and HTML.