#!/usr/bin/env bash
# Install cpuview: a private virtualenv with Textual and a `cpuview` launcher in ~/.local/bin.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
VENV="${XDG_DATA_HOME:-$HOME/.local/share}/cpuview/venv"
BIN="$HOME/.local/bin"

python3 -m venv "$VENV"
"$VENV/bin/pip" install -q --upgrade -r "$DIR/requirements.txt"

mkdir -p "$BIN"
cat > "$BIN/cpuview" <<EOF
#!/bin/sh
exec "$VENV/bin/python" "$DIR/cpuview.py" "\$@"
EOF
chmod +x "$BIN/cpuview"

echo "Installed. Run: cpuview"
case ":$PATH:" in
  *":$BIN:"*) ;;
  *) echo "Note: $BIN is not on your PATH; add it or open a new login shell." ;;
esac
