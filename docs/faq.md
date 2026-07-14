# FAQ

## Does Halfhand upload anything?

No. `hh` links no HTTP client crate and makes zero network calls (SRS NFR-2).
A CI check enforces this. Everything stays in your local data dir.

## Where is my data?

One SQLite file (`hh.db`) plus a content-addressed blob store, under your
Halfhand data dir (`~/.local/share/halfhand/` on Linux,
`~/Library/Application Support/halfhand/` on macOS). Override with
`HH_DATA_DIR`. `hh stats` summarizes it; `hh gc` reclaims space; `hh delete`
removes a session and garbage-collects its blobs.

## Can recordings contain secrets?

Yes — prompts and outputs may contain secrets you typed or the agent emitted.
`hh scan` reports what it found (type, step, location — never the secret
itself) and exits 4 when findings exist, so CI can gate on a clean scan.
`hh redact` rewrites events and blobs in place, replacing each secret with
`{{REDACTED:<type>:<hash8>}}` and securely deleting originals. Exports are
**always redacted by default**; `--no-redact` requires interactive
confirmation. See [Redaction](./redaction.md).

## Is Windows supported?

Windows builds and ships, but runtime support is best-effort / build-only per
SRS §2.2 — the PTY layer is tuned for Unix. Linux and macOS are fully
supported and CI-tested end-to-end.

## What does "faithful playback" mean?

`hh replay` reproduces the recorded transcript — agent turns, tool
inputs/outputs, file diffs, terminal output — in order. It does **not**
re-execute the agent or redo side effects. See
[Replay & Inspect](./replay-inspect.md).

## How do I share a recording?

`hh export` produces a redacted JSON bundle, a portable `.hh` archive, or a
self-contained HTML page. `hh import` brings a `.hh` bundle back into a store
under a new id. See [Export & import](./export-import.md).

## How are releases signed?

Each GitHub Release artifact has a keyless Sigstore provenance attestation
(GitHub artifact attestations). Verify with:

```bash
gh attestation verify ./hh --repo halfhandorg/halfhand
```

## How do I get shell completions?

```bash
hh completions bash   # or zsh / fish / powershell
```

See [Shell completions & man page](./completions.md).