# halfhand-org/homebrew-tap

Homebrew tap for **Halfhand** — a local-first CLI flight recorder for AI
agents. This repository holds the `hh` formula, automatically updated by
[cargo-dist](https://axo.dev/cargo-dist/) on every Halfhand release.

## Install

```bash
brew tap halfhand-org/tap
brew install hh
```

Then:

```bash
hh --version        # confirm the install
hh run -- claude    # record a Claude Code session (or any command)
hh replay last      # faithfully play it back
```

Upgrade with:

```bash
brew upgrade hh
```

## Shell completions

The formula installs the `hh` binary. Generate completions for your shell
with the built-in command (no extra package needed):

```bash
hh completions bash   > $(brew --prefix)/etc/bash_completion.d/hh
hh completions zsh    > "$(brew --prefix)/share/zsh/site-functions/_hh"
hh completions fish   > ~/.config/fish/completions/hh.fish
```

## Man page

The `hh.1` man page is published as a release asset on each
[Halfhand release](https://github.com/halfhandorg/halfhand/releases). Download
`man/hh.1` and place it on your `MANPATH`, or just run `man` against it:

```bash
curl -fsSL https://github.com/halfhandorg/halfhand/releases/latest/download/man/hh.1 -o /usr/local/share/man/man1/hh.1
mandb
man hh
```

## How this tap is updated

The `Formula/hh.rb` here is generated and pushed by cargo-dist's
`publish-jobs = ["homebrew"]` step when a `v*` tag is pushed to
`halfhandorg/halfhand`. Do not edit the formula by hand — it will be
overwritten on the next release. To release a new version, tag the
`halfhandorg/halfhand` repo; this tap updates automatically.

## More

- Halfhand source: <https://github.com/halfhandorg/halfhand>
- Docs: <https://halfhandorg.github.io/halfhand/>
- Stability policy: see STABILITY.md in the main repo