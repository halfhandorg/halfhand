# Halfhand

[![CI](https://github.com/halfhandorg/halfhand/actions/workflows/ci.yml/badge.svg)](https://github.com/halfhandorg/halfhand/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/halfhand.svg)](https://crates.io/crates/halfhand)
[![license](https://img.shields.io/crates/l/halfhand.svg)](#license)

> A local-first CLI **flight recorder** for AI agents. Record a session on your
> own machine, then replay and inspect it later — no servers, no uploads, zero
> network calls from `hh` itself.

<img src="docs/assets/halfhand-cli.png" alt="Halfhand" width="120">

Halfhand wraps any agent command in a PTY, captures terminal output and file
changes as they happen, and — for agents it recognizes — also records the
agent's internal turns (prompts, tool calls, tool results) as structured
events. Everything lands in a local SQLite database you can replay, inspect,
list, and delete. Recordings never leave your machine.

## Install

Pick one. All three install the `hh` binary.

```bash
# cargo (any OS with a Rust toolchain)
cargo install halfhand

# Homebrew (macOS / Linux)
brew tap halfhand-org/tap
brew install hh

# shell installer (prebuilt binary + SHA-256 checksum from the latest release)
curl --proto '=https' --tlsv1.2 -sSf \
  https://github.com/halfhandorg/halfhand/releases/latest/download/install.sh | sh
```

The shell installer places `hh` in `~/.halfhand/bin`; add it to your `PATH`.
Verify with `hh --version` (it reports the version **and** the git sha it was
built from). Shell completions: `hh completions bash` (or `zsh` / `fish` /
`powershell`).

## 30-second demo

```bash
hh run -- claude              # record a Claude Code session (or any command)
hh replay last                # faithfully play it back in an interactive TUI
hh inspect last               # non-interactive summary + step table
hh list                       # every recording, newest first
hh delete last --yes          # remove one
```

## Faithful playback — what it is, and what it isn't

`hh replay` is a **faithful transcript**, not a re-execution. It renders the
agent's text, tool calls, and results in order, with the exact file diffs that
happened — so you can audit, debug, or share what the agent did.

It is *not* deterministic re-execution: the agent is never re-invoked, no API
calls are made, and no side effects are reproduced. A session whose recording
was cut short (`hh` killed mid-run) replays the partial timeline and is marked
`interrupted`. For the full story, see the
[Replay & Inspect docs](docs/replay-inspect.md).

## Commands

| Command | What it does |
|---|---|
| `hh run -- <command>` | Record an agent session inside a PTY. `--record-input`, `--adapter <kind>`. |
| `hh replay <id\|last>` | Faithful playback in a TUI; `--web` exports an HTML replay. |
| `hh inspect <id\|last>` | Non-interactive detail: `--step N`, `--json`, `--diff`, `--failed`. |
| `hh list` | List sessions, newest first; `--json`, `--limit`. |
| `hh search <query>` | Full-text search over events (FTS5); `--agent`, `--kind`, `--json`. |
| `hh scan <id\|last\|--all>` | Report recorded secrets (never the secret itself); exits 4 on findings. |
| `hh redact <id\|last>` | Irreversibly remove detected secrets in place. |
| `hh export <id\|last>` | JSON bundle, portable `--bundle`, or `--html` page — **redacted by default**. |
| `hh import <file>` | Import a `--bundle` as a new local session. |
| `hh mcp-proxy -- <server>` | Wrap an MCP server in a recording stdio proxy. |
| `hh doctor` / `hh gc` / `hh stats` | Health probe / reclaim space / store summary. |
| `hh completions <shell>` | Print a shell completion script. |

Every subcommand takes `--help` with a usage example. Run `hh --help` for the
overview. Full reference: the [docs site](docs/index.md).

## Adapters

Every `hh run` captures terminal output and file changes regardless of what
you run. On top of that, Halfhand detects certain agents and records their
internal turns as structured events: **Claude Code**, **Claude Desktop**,
**OpenAI Codex CLI**, and **Google Gemini CLI** (auto-detected; force one with
`--adapter`). If a transcript can't be found, it degrades gracefully — you
still get the full terminal and file-change recording, just without the
structured breakdown. See
[Adapters](docs/adapters.md).

## Configuration

Zero config required — every setting has a sane default. `hh doctor` prints
the exact config path in effect. Config values take a literal path, not a
shell one (`~` is **not** expanded), so this creates the file with a quoted
heredoc — keeps every backslash in the redaction patterns byte-exact — and
fills in your real home directory with a trailing `sed` pass:

```bash
mkdir -p ~/.config/halfhand   # macOS: ~/Library/Application Support/halfhand
cat > ~/.config/halfhand/config.toml <<'EOF'
[record]
max_file_size = "1GiB"          # default: 4MiB
ignore = [
    "**/target/**",
    "**/.git/**",
    "**/.claude/worktrees/**",
    "**/*.rcgu.o",
    "**/*.d",
    "**/.DS_Store",
    "**/node_modules/**",
]

[replay]
theme = "auto"                   # auto | dark | light

[storage]
data_dir = "__HOME__/.local/share/halfhand"  # default already resolves here;
                                              # HH_DATA_DIR env wins over this

[redaction]
at_record = true                 # default: false — scrub secrets before they hit disk
entropy = true                   # default: true — conservative high-entropy detector
rules = [
  # Built-ins already cover AWS, GitHub, GitLab, Slack, PEM keys, and JWTs
  # (see docs/redaction.md) — these fill gaps for services Halfhand doesn't
  # know about yet.

  # ── AI / ML platforms ────────────────────────────────────────────────────
  { name = "anthropic-api-key",     pattern = "sk-ant-[a-zA-Z0-9\\-_]{32,}" },
  { name = "openai-api-key",        pattern = "sk-(?:proj|svcacct|[A-Za-z0-9]{2})-[A-Za-z0-9]{32,}" },
  { name = "google-api-key",        pattern = "AIza[0-9A-Za-z\\-_]{35}" },
  { name = "google-oauth-token",    pattern = "ya29\\.[0-9A-Za-z\\-_]{60,}" },
  { name = "huggingface-token",     pattern = "hf_[a-zA-Z0-9]{34,}" },

  # ── Payments ─────────────────────────────────────────────────────────────
  { name = "stripe-secret-key",     pattern = "sk_(?:live|test)_[a-zA-Z0-9]{24,}" },
  { name = "stripe-webhook-secret", pattern = "whsec_[a-zA-Z0-9]{32,}" },

  # ── Package registries ───────────────────────────────────────────────────
  { name = "npm-token",             pattern = "npm_[A-Za-z0-9]{36}" },
  { name = "pypi-token",            pattern = "pypi-[A-Za-z0-9_\\-]{32,}" },

  # ── Infra / DevOps ───────────────────────────────────────────────────────
  { name = "sendgrid-key",          pattern = "SG\\.[a-zA-Z0-9\\-_]{22}\\.[a-zA-Z0-9\\-_]{43}" },
  { name = "docker-pat",            pattern = "dckr_pat_[a-zA-Z0-9\\-_]{27}" },
  { name = "doppler-token",         pattern = "dp\\.pt\\.[a-zA-Z0-9]{43}" },
  { name = "vault-service-token",   pattern = "hvs\\.[a-zA-Z0-9_\\-]{90,}" },
  { name = "linear-api-key",        pattern = "lin_api_[a-zA-Z0-9]{40}" },

  # ── Database URLs with embedded credentials ──────────────────────────────
  { name = "postgres-url",          pattern = "postgres(?:ql)?://[^@\\s]*:[^@\\s]+@" },
  { name = "redis-url-auth",        pattern = "redis(?:s)?://[^@\\s]*:[^@\\s]+@" },
  { name = "mysql-url",             pattern = "mysql(?:2)?://[^@\\s]*:[^@\\s]+@" },
  { name = "mongodb-url",           pattern = "mongodb(?:\\+srv)?://[^@\\s]+:[^@\\s]+@" },
]
EOF
sed "s#__HOME__#$HOME#" ~/.config/halfhand/config.toml > ~/.config/halfhand/config.toml.tmp \
  && mv ~/.config/halfhand/config.toml.tmp ~/.config/halfhand/config.toml
```

Unknown keys warn on stderr but never fail startup; a malformed *known* key
(e.g. an invalid regex in `rules`) is a hard error, since silently dropping a
detector would be a redaction hole. Full rule-writing guide:
[Redaction](docs/redaction.md).

## Why local-first

- **Zero network calls.** `hh` links no HTTP client; nothing is uploaded. A CI
  check enforces this (SRS NFR-2).
- **You own the data.** One SQLite file plus a content-addressed blob store, in
  a data dir you control (`HH_DATA_DIR`). `hh delete` and it's gone.
- **Secrets are a first-class concern.** `hh scan` finds them; `hh redact`
  removes them in place; exports are redacted by default. See
  [Redaction](docs/redaction.md).
- **Stable interface.** 1.0 freezes the `--json` schema, keeps CLI flags
  additive, and runs forward-only DB migrations. See
  [STABILITY.md](STABILITY.md).

## Platforms

Linux and macOS are fully supported and CI-tested end-to-end. Windows builds
and ships, but runtime support is best-effort / build-only (SRS §2.2) — the
PTY layer is tuned for Unix.

## License

Apache-2.0. See [LICENSE](LICENSE). Contributions welcome — read
[CONTRIBUTING.md](CONTRIBUTING.md) before opening a PR.