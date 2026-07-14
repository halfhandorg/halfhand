# Shell completions

`hh` can print a completion script for your shell on stdout:

```bash
hh completions bash
hh completions zsh
hh completions fish
hh completions powershell
```

`hh --help` lists `completions` with a usage example (FR-6.2). The script is
plain text and pipe-safe — redirect it wherever your shell expects.

## Install per shell

### bash

```bash
hh completions bash | sudo tee /etc/bash_completion.d/hh >/dev/null
# or, per-user:
hh completions bash > ~/.local/share/bash-completion/completions/hh
```

Then start a new shell (or `source` the file).

### zsh

zsh looks in any directory on `$fpath` for files named `_hh`:

```bash
hh completions zsh > ~/.zsh/completions/_hh
# ensure the dir is on fpath (in ~/.zshrc):
#   fpath=(~/.zsh/completions $fpath)
#   autoload -Uz compinit && compinit
```

### fish

```bash
hh completions fish > ~/.config/fish/completions/hh.fish
```

fish picks it up immediately.

### PowerShell

```powershell
hh completions powershell | Out-String | Invoke-Expression
```

or save it to your `$PROFILE`:

```powershell
hh completions powershell > $PROFILE\..\hh-completions.ps1
# then dot-source it from $PROFILE
```

## In release artifacts

Pre-generated completion scripts and the `hh.1` man page are bundled in every
Halfhand release archive under `completions/` and `man/`. If you installed via
the shell installer or Homebrew, see the tap caveats; otherwise copy the files
from the archive into the locations above.

## Man page

The `hh.1` man page is generated alongside completions and shipped in release
archives. Install it manually if you built from source:

```bash
# from a release archive, or generated locally with `just dist-assets`:
cp man/hh.1 ~/.local/share/man/man1/
mandb
man hh
```