# Halfhand 

Halfhand is a local-first CLI flight recorder for AI agents. It captures an agent session on your own machine, stores it in a local SQLite database, and lets you replay or inspect it later without sending the data to a remote service.


![](docs/assets/halfhand-cli.png)

The tool is designed for privacy and reproducibility: recordings stay local, and you can point Halfhand at a custom data directory with `HH_DATA_DIR`.

## What it does

- Records agent runs from the command line.
- Captures terminal output and file changes during a session.
- Stores sessions locally so you can replay, inspect, and delete them later.
- Keeps data on disk rather than relying on a hosted service.

## Quick start

Build the CLI from the repository root:

```bash
cargo build -p hh
```

Run a recorded session:

```bash
HH_DATA_DIR=/tmp/halfhand cargo run -p hh -- run -- your-command-here
```

Example:

```bash
HH_DATA_DIR=/tmp/halfhand cargo run -p hh -- run -- python3 my_agent.py
```

## Usage

After the binary is built, the main commands are:

### 1. Record a session

```bash
hh run -- <command> [args...]
```

Examples:

```bash
hh run -- claude
hh run --record-input -- python3 my_agent.py
hh run -- --help
```

Flags:

- `--record-input` captures user keystrokes as well as terminal output.
- `--adapter <name>` forces a specific adapter instead of auto-detection.

### 2. Replay the latest session

```bash
hh replay last
```

You can also replay a specific session by id:

```bash
hh replay a1b2c3
```

### 3. Inspect a session

```bash
hh inspect last
```

Useful flags:

```bash
hh inspect last --step 7
hh inspect last --json
hh inspect last --diff
hh inspect last --failed
```

### 4. List recorded sessions

```bash
hh list
```

Useful flags:

```bash
hh list --limit 50
hh list --json
```

### 5. Delete a session

```bash
hh delete <session-id> --yes
```

Example:

```bash
hh delete last --yes
```

### 6. Run an MCP proxy session

```bash
hh mcp-proxy -- <command> [args...]
```

Example:

```bash
hh mcp-proxy -- uvx my-mcp-server
```

## Configuration

Halfhand stores its data under a directory resolved from the following sources, in order:

1. `HH_DATA_DIR`
2. A configured `[storage] data_dir` value
3. The platform default data directory

Example:

```bash
HH_DATA_DIR=/tmp/halfhand hh list
```

## Current status

This repository snapshot contains the CLI skeleton and the `hh run` recording flow. The other subcommands are part of the roadmap and may return a “not implemented” message in the current build until those pieces are wired up.
