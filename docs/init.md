# Setup (`hh init`)

`hh init` sets Halfhand up explicitly and tells you where everything is:

```console
$ hh init
✓ Halfhand is set up

  config    created  /home/me/.config/halfhand/config.toml
  data dir  created  /home/me/.local/share/halfhand
  database  created  /home/me/.local/share/halfhand/hh.db
  blobs     created  /home/me/.local/share/halfhand/blobs
  exports   created  /home/me/.local/share/halfhand/exports

● Sessions hold prompts and source code, so new directories are private (mode 0700).

Next: hh run -- <agent>   e.g. hh run -- claude
```

It is **optional**. Every other command creates the data directory and
database on first use, so if you never run `hh init`, nothing changes for you.
What `hh init` adds:

- **It prints every path, labelled and absolute.** The paths shown are the
  ones currently in effect, including any `HH_DATA_DIR` or
  `[storage] data_dir` override.
- **It writes a commented starter `config.toml`** if you have no config file
  yet. Every key is listed with its purpose and its real default, and the
  ignore list is already filled in (`**/target/**`, `**/.git/**`,
  `**/node_modules/**`, `**/.DS_Store`, `**/.claude/worktrees/**`). Edit it,
  or delete lines you don't need.
- **It creates the data directory, blob store, exports directory and
  database.** Database migrations run as part of this. On Linux and macOS
  every directory it creates is mode `0700`, because sessions contain your
  prompts and source code.

## Running it again

`hh init` is idempotent. A second run creates nothing, changes nothing, and
says so:

```console
$ hh init
✓ Halfhand is already set up; nothing changed

  config    found  /home/me/.config/halfhand/config.toml
  ...
```

**An existing config is never overwritten.** To start over from the template,
use `--force`. It first copies your current file to `config.toml.bak` (next to
`config.toml`, replacing any older backup), then writes the template:

```bash
hh init --force
```

## Where things live

Halfhand uses each platform's standard directories. By default:

| Platform | Config file | Data directory |
|---|---|---|
| Linux | `~/.config/halfhand/config.toml` | `~/.local/share/halfhand/` |
| macOS | `~/Library/Application Support/halfhand/config.toml` | `~/Library/Application Support/halfhand/` (**the same directory**) |
| Windows | `%APPDATA%\halfhand\config\config.toml` | `%APPDATA%\halfhand\data\` (roaming profile) |

On Linux, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` move these as usual. A unit
test checks this table against the code, so it stays accurate.

The data directory holds:

| Path | What it is |
|---|---|
| `hh.db` | The SQLite database: sessions, events, search index |
| `blobs/` | Content-addressed file contents and large outputs |
| `exports/` | HTML pages from `hh export --html` and `hh replay --web` |

The data directory is chosen in this order:

1. `HH_DATA_DIR` in the environment.
2. `[storage] data_dir` in the config file.
3. The platform default from the table above.

The config file's location is always the platform default.

### macOS: config and data share one directory

On macOS the config file sits **inside** the data directory. If you delete
`~/Library/Application Support/halfhand` to clear your sessions, your
`config.toml` goes with it. `hh init` warns about this whenever the two
directories are the same. To remove sessions, use `hh delete` (one session)
and `hh gc` (reclaim space) instead, or move the data elsewhere with
`HH_DATA_DIR` or `[storage] data_dir`.

## Which config file is read

Halfhand looks in the config directory for:

1. `config.toml`, the canonical name;
2. otherwise `halfhand.toml`, then `hh.toml` (legacy names);
3. otherwise no file at all, and the built-in defaults apply.

If you only have a `halfhand.toml`, `hh init` keeps using it and does not write
a `config.toml` next to it, because a new `config.toml` would silently replace
it. Instead, it tells you which file is being read:

```console
● reading /home/me/.config/halfhand/halfhand.toml (config.toml does not exist). Run `hh init --migrate-config` to rename it to config.toml.
```

### Migrating `halfhand.toml`

```bash
hh init --migrate-config
```

This renames `halfhand.toml` (or `hh.toml`) to `config.toml` without changing
its contents. If `config.toml` already exists, nothing is renamed. In that case
the legacy file is being ignored, and `hh init` and `hh doctor` both point that
out.

`--force` refuses to run while a legacy file is the one in use. Combine the two
flags to rename the file and then replace it with the template:
`hh init --migrate-config --force`. Your old settings are kept in
`config.toml.bak`.

## Checking setup with `hh doctor`

`hh doctor` adds one informational line saying whether everything `hh init`
creates already exists, and if not, what it would create:

```console
● init — not run; `hh init` would create: config /home/me/.config/halfhand/config.toml, exports /home/me/.local/share/halfhand/exports
```

`hh doctor --json` adds an `init` object. It is purely additive: no existing
field changes, and it never affects the exit code.

```json
"init": {
  "initialized": false,
  "would_create": [
    { "label": "config", "path": "/home/me/.config/halfhand/config.toml" }
  ]
}
```

## Exit codes

| Code | When |
|---|---|
| `0` | Setup succeeded, including "nothing changed". |
| `1` | A directory, the database or the config file could not be created, read, renamed or backed up; an existing config file is malformed (fix it or use `--force`); or `--force` was used while `halfhand.toml` is the file in use. |
| `2` | Invalid command line, such as an unknown flag. |

See [Exit codes](./exit-codes.md).
