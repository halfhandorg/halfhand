# Releasing Halfhand

The exact runbook for cutting a release. **The maintainer tags and publishes;
Halfhand's automation does not tag itself.** Do not run the publish or tag
steps until `docs/manual-qa.md` has passed on Linux and macOS (and Windows
build-only) and `docs/1.0-definition-of-done.md` is green.

> **Never commit a crates.io token.** `cargo publish` reads
> `~/.cargo/credentials` (set via `cargo login` once, interactively, on the
> releasing machine). If a token has been pasted into a terminal or chat,
> **rotate it** at <https://crates.io/settings/tokens> before publishing.
> `cargo publish --dry-run` needs no token and uploads nothing — use it freely.

## 0. Pre-flight (run on the release branch, no token needed)

```bash
# Clean working tree, on main, up to date.
git status --porcelain         # expect: clean (or only release edits)
git pull --ff-only

# The full gate — all must pass.
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo doc --workspace --no-deps -- -D warnings
cargo deny check
cargo audit                    # reads audit.toml; expect "N allowed warnings"
cargo semver-checks check-release -p hh-core --baseline-rev origin/main \
  --release-type minor --exclude enum_marked_non_exhaustive
```

Coverage floor (`hh-core` ≥ 80% lines) and the bench regression gate (>15%
median-time regression fails) run in CI, not here — confirm the latest
`coverage` and `bench (nightly)` workflow runs are green.

## 1. Bump the version (if not already bumped)

The workspace version lives in `Cargo.toml` (`[workspace.package]` and
`[workspace.dependencies]`). For 1.0.0 it is already `1.0.0`. For a later
release, bump all three (`hh-core`, `hh-record`, `halfhand`) together — they
share `version.workspace = true` — and bump the inter-crate version
constraints in `[workspace.dependencies]` to match.

```bash
grep -n 'version = ' Cargo.toml                      # workspace + deps
grep -rn '0.1.0-beta.1\|"1.0.0"' hh/tests/cli.rs     # the version-asserts test must match
```

The integration test `hh/tests/cli.rs::version_prints_pkg_version` asserts the
printed version contains the package version string — update it if you bump.

## 2. Update CHANGELOG and release notes

- Move the `[Unreleased]` block to a dated `## [X.Y.Z] — YYYY-MM-DD` entry.
- Add a curated **Highlights** block at the top of the new entry.
- Update the link references at the bottom of `CHANGELOG.md`.
- Update `RELEASE_NOTES_X.Y.Z.md` (copy `RELEASE_NOTES_1.0.0.md` as a template).

## 3. Verify packaging (dry-run, no token, no upload)

Workspace crates depend on each other. `cargo publish --dry-run` for a
downstream crate resolves its inter-crate version constraint against
**crates.io**, not the path — so it only passes once the upstream crate is
actually published. Dry-run the leaf first:

```bash
# hh-core has no unpublished internal deps — this proves packaging works.
cargo publish --dry-run --allow-dirty -p hh-core
#   expect: "warning: aborting upload due to dry run"
```

`hh-record` and `halfhand` dry-runs will fail with "failed to select a version
for the requirement `hh-core = "^1.0.0`" **until hh-core 1.0.0 is published**
(step 6). That is expected, not a problem — the leaf dry-run plus the full
`cargo test --workspace` (which uses the path deps) together prove the
crates build. Re-run the downstream dry-runs after each upstream publishes.

## 4. Commit and push

```bash
git checkout -b release/1.0.0
git add -A
git commit -m "release: Halfhand 1.0.0

Bump workspace + inter-crate versions to 1.0.0, add the 1.0.0 CHANGELOG
entry and release notes, and add the beta→1.0 migration test + fixture.

Co-Authored-By: Claude <noreply@anthropic.com>"
git push -u origin release/1.0.0
```

Open a PR, wait for CI green (fmt, clippy, test on Linux/macOS, build-windows,
deny, coverage, semver-checks), and merge to `main`.

## 5. Tag (maintainer only, after manual QA passes)

**Do not tag until `docs/manual-qa.md` is signed off.** When it is:

```bash
git checkout main && git pull --ff-only
git tag -a v1.0.0 -m "Halfhand 1.0.0"
git push origin v1.0.0
```

Pushing the `v*` tag triggers cargo-dist's generated release workflow
(`.github/workflows/release.yml`): per-target binaries, SHA-256 checksums,
shell + powershell installers, the Homebrew formula push to
`halfhand-org/homebrew-tap`, and keyless GitHub artifact attestations. Watch
it under the **Actions** tab.

## 6. Publish to crates.io (maintainer only, one-time `cargo login`)

Publish in dependency order — each downstream crate becomes publishable only
after its upstream lands on crates.io:

```bash
cargo login              # once, interactively; paste the token. Never commit it.
cargo publish -p hh-core
cargo publish -p hh-record    # only after hh-core is live
cargo publish -p halfhand     # only after hh-record is live
# hh-dist is publish = false — do not publish it.
```

After each `cargo publish`, wait for the crate to appear on crates.io before
publishing the next (the index propagates in a few seconds to a minute). If a
publish fails because the version already exists, that crate is already live —
continue with the next.

### Sanity-check the published binary

```bash
cargo install halfhand
hh --version              # expect: hh 1.0.0 (<sha>)
hh --help
```

## 7. Publish the GitHub Release

Once the cargo-dist workflow finishes:

- Create (or finalize) the GitHub Release for `v1.0.0` and paste the body from
  `RELEASE_NOTES_1.0.0.md`.
- Confirm the artifacts attached include the installers, per-target archives,
  and `sha256sums.txt`.
- Verify a binary's provenance:
  ```bash
  gh attestation verify ./hh --repo halfhandorg/halfhand
  ```
- Confirm the Homebrew formula landed: `brew tap halfhand-org/tap && brew info hh`.

## 8. Publish the docs site

The `docs.yml` workflow deploys mdbook to GitHub Pages on release tags. Confirm
`https://halfhandorg.github.io/halfhand/` reflects the new version. If it
didn't trigger, run the workflow manually from the Actions tab.

## 9. Announce

Post the release notes (trimmed) to the channels you use, and record the
`docs/manual-qa.md` pass/fail per line in the announcement. Note any
**N/A** (e.g. Codex unavailable on a given OS).

## Rollback

A published crates.io version **cannot be unpublished** (it can be yanked,
which stops new resolves from picking it up but keeps it available to
existing lockfiles). Before publishing, the dry-run (step 3) and the full
test suite (step 0) are the guardrails. If a published crate is broken, yank
it (`cargo yank --vers 1.0.0 -p halfhand`) and cut a `1.0.1` patch. A bad git
tag can be deleted and re-pushed only before the release workflow uploads
artifacts — after that, treat the tag as immutable and go the patch route.