#!/usr/bin/env bash
# Build cpuview_<version>_<arch>.deb into dist/.
#   packaging/build-deb.sh            amd64 (x86_64)
#   packaging/build-deb.sh arm64      aarch64 (needs: rustup target add aarch64-unknown-linux-musl)
# The binary is static (musl): one .deb runs on every Debian / Ubuntu release and derivative,
# with no library dependencies.
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ARCH="${1:-amd64}"
case "$ARCH" in
  amd64) TARGET=x86_64-unknown-linux-musl ;;
  arm64) TARGET=aarch64-unknown-linux-musl ;;
  *) echo "unknown arch: $ARCH (amd64 or arm64)" >&2; exit 1 ;;
esac
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$DIR/Cargo.toml" | head -1)"
REV="${DEB_REVISION:-1}"
if [ "$ARCH" = arm64 ]; then
  # rust-lld links static musl binaries for any target, no cross gcc needed
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
fi
cargo build --release --locked --target "$TARGET" --manifest-path "$DIR/Cargo.toml"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
R="$STAGE/root"
install -Dm755 "$DIR/target/$TARGET/release/cpuview" "$R/usr/bin/cpuview"
install -d "$R/usr/share/cpuview/wallpapers"
install -m644 "$DIR"/wallpapers/*.jpg "$DIR"/wallpapers/*.png "$DIR/wallpapers/CREDITS.md" \
  "$DIR/wallpapers/credits.toml" "$R/usr/share/cpuview/wallpapers/"
D="$R/usr/share/doc/cpuview"
install -d "$D"
gzip -9n < "$DIR/README.md" > "$D/README.md.gz"
cat > "$D/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: cpuview
Source: https://github.com/aipulsedaily/cpuview

Files: *
Copyright: cpuview contributors
License: MIT

Files: usr/share/cpuview/wallpapers/*
Copyright: see /usr/share/cpuview/wallpapers/CREDITS.md
License: public-domain or CC0-1.0
 NASA photographs are public domain; the generated pictures are CC0 1.0. Each
 file's source, author and licence is listed in CREDITS.md.

License: MIT
$(sed 's/^$/./; s/^/ /' "$DIR/LICENSE")
EOF
printf 'cpuview (%s-%s) stable; urgency=medium\n\n  * Release %s.\n\n -- cpuview contributors <noreply@cpuview.invalid>  %s\n' \
  "$VERSION" "$REV" "$VERSION" "$(date -R -d @"${SOURCE_DATE_EPOCH:-$(git -C "$DIR" log -1 --format=%ct)}")" \
  | gzip -9n > "$D/changelog.Debian.gz"

install -d "$R/DEBIAN"
cat > "$R/DEBIAN/control" <<EOF
Package: cpuview
Version: $VERSION-$REV
Architecture: $ARCH
Maintainer: cpuview contributors <noreply@cpuview.invalid>
Installed-Size: $(du -sk --exclude=DEBIAN "$R" | cut -f1)
Suggests: lm-sensors
Section: utils
Priority: optional
Homepage: https://github.com/aipulsedaily/cpuview
Description: terminal system monitor for the hardware and its health
 A btop-style monitor focused on the machine rather than a wall of processes:
 every core's clock and load, temperatures, fans, power, RAM, disks, network
 and containers, with themes and full-colour picture backgrounds (sixel,
 iTerm2 and kitty graphics). It costs under 0.01 % of one CPU thread in eco
 mode and runs at 60 fps in smooth mode.
 .
 Sensors the kernel doesn't expose yet (a board's fan chip, CPU power for
 non-root users) can be enabled with: sudo cpuview --setup-sensors
EOF

mkdir -p "$DIR/dist"
OUT="$DIR/dist/cpuview_${VERSION}-${REV}_${ARCH}.deb"
dpkg-deb --root-owner-group -Zxz --build "$R" "$OUT" >/dev/null
echo "$OUT ($(du -h "$OUT" | cut -f1))"
