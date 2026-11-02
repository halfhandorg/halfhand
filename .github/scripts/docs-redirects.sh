#!/bin/sh
# Writes redirect stubs for the retired mdbook site at
# halfhandorg.github.io/halfhand/ into OUT_DIR. Every old page
# (<name>.html, adr/<name>.html) points at its halfhand.org/docs equivalent;
# contributor-only pages that halfhand.org does not publish point at the file
# on GitHub instead.
#
# Usage: docs-redirects.sh <docs-dir> <out-dir>
set -eu

docs_dir=${1:?usage: docs-redirects.sh <docs-dir> <out-dir>}
out_dir=${2:?usage: docs-redirects.sh <docs-dir> <out-dir>}
site=https://halfhand.org/docs
repo=https://github.com/halfhandorg/halfhand/blob/main/docs

stub() {
  # stub <output file> <target url>
  mkdir -p "$(dirname "$1")"
  cat > "$1" <<HTML
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Halfhand docs have moved</title>
<link rel="canonical" href="$2">
<meta http-equiv="refresh" content="0; url=$2">
</head>
<body>
<p>The Halfhand docs now live at <a href="$2">$2</a>.</p>
</body>
</html>
HTML
}

rm -rf "$out_dir"
mkdir -p "$out_dir"

count=0
for md in $(cd "$docs_dir" && find . -name '*.md' ! -name SUMMARY.md | sed 's#^\./##' | sort); do
  page=${md%.md}
  case $page in
    index) target=$site/ ;;
    1.0-definition-of-done | manual-qa | homebrew-tap-README) target=$repo/$md ;;
    *) target=$site/$page/ ;;
  esac
  stub "$out_dir/$page.html" "$target"
  count=$((count + 1))
done

# mdbook extras that old links may point at.
stub "$out_dir/print.html" "$site/"
stub "$out_dir/404.html" "$site/"
touch "$out_dir/.nojekyll"

# Older docs told people to `curl .../halfhand/install.sh | sh`. Keep that
# command working by forwarding to the cargo-dist installer on the latest
# release (the same thing https://halfhand.org/install.sh does).
cat > "$out_dir/install.sh" <<'SH'
#!/bin/sh
# Moved: use https://halfhand.org/install.sh
set -eu
curl --proto '=https' --tlsv1.2 -LsSf \
    https://github.com/halfhandorg/halfhand/releases/latest/download/halfhand-installer.sh | sh
SH

echo "wrote $count page redirects to $out_dir"
