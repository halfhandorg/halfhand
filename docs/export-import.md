# `hh export` / `hh import` / `hh replay --web`

Three ways to move a session off the recording machine:

| Goal | Command |
| --- | --- |
| Machine-readable JSON (`jq`-friendly) | `hh export [session]` |
| A portable archive you can `hh import` elsewhere | `hh export [session] --bundle -o FILE` |
| A page to share/open in a browser | `hh export [session] --html -o FILE`, or `hh replay [session] --web [--open]` |

All three are **redacted by default** — see [`docs/redaction.md`](redaction.md).
`--no-redact` requires an interactive typed confirmation and is refused on a
non-TTY stdin; there is no bypass flag, so a script can never exfiltrate a raw
session by accident.

```
$ hh export last --bundle -o session.hh
✓ Exported session a1b2c3 → session.hh (bundle, redacted)

$ hh import session.hh
✓ Imported session f9e8d7 (from a1b2c3) · 12 steps · hh replay f9e8d7

$ hh replay last --web
✓ Exported session a1b2c3 → ~/.local/share/halfhand/exports/hh-session-a1b2c3.html (html, redacted) — open it in a browser
```

## The bundle format (`--bundle`)

A `.hh` file is a single **zstd-compressed tar**:

```
manifest.json     format_version, hh_version, the session block, integrity hashes
events.ndjson     one compact JSON object per event, seq-indexed
blobs/<h2>/<hash> raw content for every blob any event or file_change references
```

Only the outer tar stream is compressed — blobs are stored raw inside it (no
double compression). Building it twice from an unchanged session produces
**byte-identical bytes**: tar entries are added in a fixed order (manifest,
events, then blobs sorted ascending by hash) with fixed mtime/uid/gid/mode,
and the manifest carries no wall-clock field.

### `manifest.json`

```jsonc
{
  "format_version": 1,
  "hh_version": "0.1.0-beta.1 (abc1234)",
  "session": { /* the hh list --json session object, see docs/json.md */ },
  "integrity": {
    "events_blake3": "<hex>",       // BLAKE3 of events.ndjson's bytes
    "event_count": 12,
    "blobs": [ { "hash": "<hex>", "size": 481 }, ... ]
  }
}
```

`hh import` rejects a bundle whose `format_version` is newer than the highest
version that build of `hh` understands, with an actionable "upgrade hh"
message. Same-or-older versions of format `1` are accepted (the only version
that exists today).

### `events.ndjson`

One line per event, in the session's original `(ts_ms, id)` order. DB row ids
don't survive import, so events are addressed by position (`seq`, 0-based)
instead, and `correlates` becomes `correlates_seq`:

```jsonc
{
  "seq": 3,
  "ts_ms": 1204,
  "kind": "tool_result",
  "step": 2,
  "correlates_seq": 2,
  "summary": "tool_result: ok",
  "body": { "content": "file.txt", "is_error": false },
  "blob_hash": null,
  "blob_size": null,
  "file_change": null
}
```

`body` is exactly as stored — unlike `hh inspect --json`'s event object, it
is **not** blob-overflow-resolved, so a large payload may still carry the
`{"overflow": true, "blob_hash": "...", ...}` envelope; the referenced blob
is always present under `blobs/` in that case.

### Blobs

Every hash any event's `blob_hash` or `file_change.before_hash`/`after_hash`
references is packed under `blobs/<hash[0..2]>/<hash>`, content-addressed and
BLAKE3-verified on import. This is the one thing plain `hh export` (no
`--bundle`) cannot carry: its JSON only ever includes hash *references* for
file changes, never the bytes.

### Redaction and hash remapping

Redaction (when enabled) runs over the session block, every event, and every
referenced blob's content — the same chokepoint plain `hh export`/`hh export
--html` use. If redacting a blob's content changes its hash, every reference
to the old hash (the event's `blob_hash`, a `file_change`'s
`before_hash`/`after_hash`, or an embedded `blob_hash` inside an unresolved
overflow envelope) is rewritten to the new one before the bundle is built —
nothing in the bundle ever points at a hash that isn't in it.

## `hh import`

```
hh import FILE
```

Validates the bundle end to end before touching the store: zstd
decompression is bounded (defense against a hostile/corrupt frame), every
tar entry must match the allow-list above (a symlink/hardlink/unexpected
path is refused), every blob's content is BLAKE3-verified against its
filename, `events.ndjson`'s digest is checked against the manifest, and
every hash any event references must be present among the bundle's blobs.
Each failure mode reports precisely what's wrong — not one generic "corrupt
bundle" message.

On success, the session is imported under a **brand-new local id** — the
bundle's `format_version` and content-addressing mean imports are always
additive, never overwrite an existing session. The original session id is
recorded in the new session's `imported_from` field (`hh list --json` /
`hh inspect --json`; see [`docs/json.md`](json.md)) so you can trace where it
came from. Blobs are written content-addressed, so importing a bundle whose
blobs already exist locally (e.g. re-importing, or two sessions sharing a
file snapshot) is a no-op for those blobs.

`hh import` does **not** re-run redaction — it trusts the bundle's existing
redaction state, whatever `hh export` produced (redacted by default, or
`--no-redact` if the exporter explicitly confirmed that).

### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Import succeeded. |
| `1` | The file couldn't be read, the bundle failed validation, or the import transaction failed. |
| `2` | Usage error (clap). |

There is no dedicated "corrupt bundle" exit code in Halfhand's documented
contract (`0`/`1`/`2`/`3`/`4`, see [Exit codes](./exit-codes.md)); a corrupt
or invalid bundle is a generic `1`, by design — this is called out
explicitly rather than left as an oversight. `hh export` exits `3` when the
session doesn't exist, and `1` when `--no-redact` is refused for lack of an
interactive confirmation.

## The HTML replay page (`--html`, `hh replay --web`)

`hh export --html` (and its sugar, `hh replay --web`) produce a **single
self-contained HTML file**: inline CSS/JS, zero network requests, no CDN, no
telemetry, safe to open from `file://` or host anywhere.

- **Header**: session id, status, agent kind, command, cwd, and (if
  applicable) which session it was imported from.
- **Terminal output**: a collapsed "terminal output · N KiB" section under
  the header, expanded with a click (rendered lazily on first open). This is
  everything the agent wrote to its terminal, in order — for sessions with
  no structured transcript (any agent other than Claude Code) it is most of
  the session. ANSI colors and bold/dim/italic/underline become styles;
  every other escape sequence (cursor movement, window titles, OSC 8
  hyperlinks) is stripped, `\r` progress-bar redraws collapse to their
  final state, and keystrokes recorded with `--record-input` are left out.
  Non-UTF-8 output chunks are not decoded; the summary line reports how many
  bytes were omitted.
- **Timeline**: every other event, clickable. Terminal output chunks are not
  timeline rows (they are not steps).
- **Detail pane**: plain text for text-bearing events, pretty-printed JSON
  for structured events, and a real diff view for file changes (hunks are
  precomputed server-side, so the page never implements a diff algorithm).
- **Keyboard nav**: `j`/`k` (and arrow keys) move the selected step.
- **Theme**: dark by default, with a toggle button for light — not tied to
  the OS `prefers-color-scheme`.

### How the page stays safe

Session content is untrusted input to this page (it's whatever the recorded
agent/tool output happened to contain). The whole payload is embedded once,
inside a single `<script type="application/json" id="hh-data">` tag, with
every `<`/`>`/`&` replaced by its `\uXXXX` escape first — that makes a
`</script>` sequence structurally impossible to smuggle in, regardless of
content. The page's hand-written JS reads that tag's `textContent` (never
`innerHTML`), `JSON.parse`s it, and builds every other DOM node with
`createElement`/`textContent`. Nothing recorded is ever assigned to
`innerHTML`, so nothing recorded can ever be interpreted as markup — a
`<script>` tag or `onerror=` attribute in a summary, a tool result, a
diff line, or terminal output renders as inert text, never as live HTML.

Terminal output gets two more guards: escape sequences are parsed and
discarded server-side (none reach the page), and each styled run's CSS
class is built from a fixed `hh-t-*` vocabulary and re-checked by the page
against that pattern before use.

### Redaction and size budget for terminal output

Terminal output is redacted like everything else on the page — and it is
redacted on the **joined** transcript, so a secret split across two output
chunks, or by a color change mid-token, is still caught.

Terminal output is usually the largest part of a session, so the page caps
it at **2 MiB** by default (the whole page stays within ~5 MiB for a typical
session). Past the cap, the page keeps the head and tail of the transcript
around a visible `… N truncated — raise [export] html_terminal_max_bytes …`
marker. Truncation happens *after* redaction, so a cut can never split a
secret into halves that escape detection. Raise or remove the cap in
`config.toml`:

```toml
[export]
html_terminal_max_bytes = "8MiB"   # or an integer byte count; 0 = no limit
```

### Where the file goes, and `--open`

| Invocation | Output |
| --- | --- |
| `hh replay [session] --web` | `<data_dir>/exports/hh-session-<short_id>.html`; path printed on stdout |
| `hh replay [session] --web -o page.html` | `page.html`; path printed on stdout |
| `hh replay [session] --web -o -` | HTML on stdout, nothing else |
| `hh export [session] --html` | HTML on stdout (unchanged) |
| `hh export [session] --html -o page.html` | `page.html`; path printed on stdout |
| `hh export [session] --html -o -` | HTML on stdout (`-o -` works for every export format) |
| `hh export [session] --html --open` | `<data_dir>/exports/hh-session-<short_id>.html`, then opened |

`<data_dir>` is the Halfhand data directory (`HH_DATA_DIR`, `[storage]
data_dir`, or the platform default — `hh doctor` prints it). Pages there are
not reaped by the OS like a temp dir; re-exporting a session overwrites its
page, and `rm -r <data_dir>/exports` is always safe.

`--open` (with `--html` or `--web`) opens the page in your default browser
(`xdg-open` on Linux, `open` on macOS). It is honoured **only** when stdout
is a terminal and the `CI` environment variable is not set; otherwise the
page is still written, the path is printed, and one line on stderr says why
no browser opened. If no opener is available (headless Linux, an SSH
session), the command still succeeds and prints the path with a one-line
hint. `--no-open` cancels an earlier `--open` (handy behind a shell alias).

The stdout epilogue line is unchanged — `✓ Exported session <id> → <path>
(html, redacted)` — so scripts that parse the path after `→ ` keep working.

### `hh replay --web`

Sugar for the common "just let me look at this in a browser" case:

```
hh replay [session] --web [--open] [-o FILE|-]
```

Builds the same redacted HTML page `hh export --html` would, writes it to
`<data_dir>/exports/hh-session-<short_id>.html` (earlier versions used
`$TMPDIR/hh-replay-<short_id>.html`, which the OS could delete), and prints
the path. Needs no terminal (unlike plain `hh replay`, which opens the
interactive TUI and requires one), and never starts a server. Add `--open`
to launch your browser too (see above).

## See also

- [`docs/redaction.md`](redaction.md) / [`docs/redaction-design.md`](redaction-design.md) — the redaction pipeline every export path shares.
- [`docs/json.md`](json.md) — the session/event object shapes referenced above.
