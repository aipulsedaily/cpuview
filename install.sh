#!/usr/bin/env bash
# Build cpuview (Rust) and install it for the current user:
#   ~/.local/bin/cpuview                       the binary
#   ~/.local/share/cpuview/wallpapers/         the bundled wallpaper pack (+ credits)
# An existing Python launcher at ~/.local/bin/cpuview is kept as ~/.local/bin/cpuview-py.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="$HOME/.local/bin"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}/cpuview"

if ! command -v cargo >/dev/null 2>&1; then
  if [ -x "$HOME/.cargo/bin/cargo" ]; then
    PATH="$HOME/.cargo/bin:$PATH"
  else
    echo "cargo not found: install Rust from https://rustup.rs first" >&2
    exit 1
  fi
fi

cargo build --release --manifest-path "$DIR/Cargo.toml"

mkdir -p "$BIN"
if [ -e "$BIN/cpuview" ] && ! head -c 4 "$BIN/cpuview" | grep -q $'\x7fELF'; then
  # the old Python launcher: keep it runnable as cpuview-py
  if [ ! -e "$BIN/cpuview-py" ]; then
    sed "s#$DIR/cpuview.py#$DIR/legacy/python/cpuview.py#" "$BIN/cpuview" > "$BIN/cpuview-py"
    chmod +x "$BIN/cpuview-py"
    echo "Kept the Python version as $BIN/cpuview-py"
  fi
fi
install -m 755 "$DIR/target/release/cpuview" "$BIN/cpuview.new"
mv -f "$BIN/cpuview.new" "$BIN/cpuview"

mkdir -p "$DATA/wallpapers"
cp "$DIR"/wallpapers/*.jpg "$DIR"/wallpapers/*.png "$DIR/wallpapers/CREDITS.md" "$DIR/wallpapers/credits.toml" "$DATA/wallpapers/"

echo "Installed $BIN/cpuview ($(du -h "$BIN/cpuview" | cut -f1)) and wallpapers in $DATA/wallpapers"
echo "Run: cpuview        (cpuview --doctor shows what your terminal supports)"
case ":$PATH:" in
  *":$BIN:"*) ;;
  *) echo "Note: $BIN is not on your PATH; add it or open a new login shell." ;;
esac
