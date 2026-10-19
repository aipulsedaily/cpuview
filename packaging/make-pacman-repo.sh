#!/usr/bin/env bash
# Build the Arch package from dist/cpuview-<version>-x86_64-linux.tar.gz and a signed pacman
# repository in OUT (default: dist/apt/arch/x86_64, served next to the APT repository).
# Runs on Arch Linux as a normal user (makepkg, repo-add):
#   packaging/make-pacman-repo.sh KEY_ID [OUT]
# Users add the key and the repository once, then `pacman -S cpuview`:
#   curl -fsSLo /tmp/cpuview.gpg https://<host>/cpuview.gpg
#   sudo pacman-key --add /tmp/cpuview.gpg && sudo pacman-key --lsign-key KEY_ID
#   printf '\n[cpuview]\nServer = https://<host>/arch/$arch\n' | sudo tee -a /etc/pacman.conf
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KEY="${1:?usage: make-pacman-repo.sh KEY_ID [OUT]}"
OUT="${2:-$DIR/dist/apt/arch/x86_64}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$DIR/Cargo.toml" | head -1)"
TARBALL="$DIR/dist/cpuview-$VERSION-x86_64-linux.tar.gz"
[ -f "$TARBALL" ] || { echo "missing $TARBALL (packaging/build-tarball.sh)" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cp "$TARBALL" "$WORK/"
sed -e "s/^pkgver=.*/pkgver=$VERSION/" \
    -e "s/^sha256sums=.*/sha256sums=('$(sha256sum "$TARBALL" | cut -d' ' -f1)')/" \
    "$DIR/packaging/arch/PKGBUILD" > "$WORK/PKGBUILD"
(cd "$WORK" && PKGDEST="$WORK/out" SOURCE_DATE_EPOCH="$(git -C "$DIR" log -1 --format=%ct)" \
  makepkg --nodeps --noconfirm --clean >/dev/null)

mkdir -p "$OUT"
for p in "$WORK"/out/*.pkg.tar.zst; do
  cp "$p" "$OUT/"
  gpg --batch --yes --local-user "$KEY" --detach-sign -o "$OUT/$(basename "$p").sig" "$p"
done
# the database and its files list, as real files (static hosting doesn't follow symlinks)
(cd "$OUT" && repo-add --quiet --sign --key "$KEY" cpuview.db.tar.gz ./*.pkg.tar.zst)
for f in cpuview.db cpuview.files cpuview.db.sig cpuview.files.sig; do
  [ -L "$OUT/$f" ] && cp --remove-destination "$(readlink -f "$OUT/$f")" "$OUT/$f"
done
echo "pacman repository in $OUT"
ls "$OUT"
