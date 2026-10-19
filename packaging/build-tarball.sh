#!/usr/bin/env bash
# Build dist/cpuview-<version>-<arch>-linux.tar.gz: the static (musl) binary, the wallpaper
# pack, LICENSE and README, for any Linux distribution (and the Arch package's source).
#   packaging/build-tarball.sh            x86_64
#   packaging/build-tarball.sh aarch64
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCH="${1:-x86_64}"
case "$ARCH" in
  x86_64 | aarch64) TARGET="$ARCH-unknown-linux-musl" ;;
  *) echo "unknown arch: $ARCH (x86_64 or aarch64)" >&2; exit 1 ;;
esac
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
[ "$ARCH" = aarch64 ] && export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
cargo build --release --locked --target "$TARGET" --manifest-path "$DIR/Cargo.toml"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$DIR/Cargo.toml" | head -1)"
NAME="cpuview-$VERSION-$ARCH-linux"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
R="$STAGE/$NAME"
install -Dm755 "$DIR/target/$TARGET/release/cpuview" "$R/bin/cpuview"
install -d "$R/share/cpuview/wallpapers"
install -m644 "$DIR"/wallpapers/*.jpg "$DIR"/wallpapers/*.png "$DIR/wallpapers/CREDITS.md" \
  "$DIR/wallpapers/credits.toml" "$R/share/cpuview/wallpapers/"
install -m644 "$DIR/LICENSE" "$DIR/README.md" "$R/"

mkdir -p "$DIR/dist"
OUT="$DIR/dist/$NAME.tar.gz"
tar --owner=0 --group=0 --numeric-owner --sort=name \
  --mtime="@${SOURCE_DATE_EPOCH:-$(git -C "$DIR" log -1 --format=%ct)}" \
  -C "$STAGE" -czf "$OUT" "$NAME"
echo "$OUT ($(du -h "$OUT" | cut -f1))"
