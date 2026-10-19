#!/usr/bin/env bash
# Build a signed APT repository from the .debs in dist/ into OUT (default: dist/apt),
# ready to serve as static files (GitHub Pages, any web server):
#   packaging/make-apt-repo.sh KEY_ID [OUT]
# KEY_ID is the GPG key that signs the repository (gpg --list-secret-keys); its public half
# is exported as OUT/cpuview.gpg for users to install. Users then run:
#   curl -fsSL https://<host>/cpuview.gpg | sudo tee /usr/share/keyrings/cpuview.gpg >/dev/null
#   echo "deb [signed-by=/usr/share/keyrings/cpuview.gpg] https://<host> stable main" \
#     | sudo tee /etc/apt/sources.list.d/cpuview.list
#   sudo apt update && sudo apt install cpuview
set -euo pipefail

DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
KEY="${1:?usage: make-apt-repo.sh KEY_ID [OUT]}"
OUT="${2:-$DIR/dist/apt}"

rm -rf "$OUT"
mkdir -p "$OUT/pool/main/c/cpuview"
cp "$DIR"/dist/cpuview_*.deb "$OUT/pool/main/c/cpuview/"
cd "$OUT"
ARCHES=$(for d in pool/main/c/cpuview/*.deb; do dpkg-deb -f "$d" Architecture; done | sort -u)
for a in $ARCHES; do
  B="dists/stable/main/binary-$a"
  mkdir -p "$B"
  apt-ftparchive --arch "$a" packages pool > "$B/Packages"
  gzip -9nk "$B/Packages"
done
apt-ftparchive \
  -o APT::FTPArchive::Release::Origin=cpuview \
  -o APT::FTPArchive::Release::Label=cpuview \
  -o APT::FTPArchive::Release::Suite=stable \
  -o APT::FTPArchive::Release::Codename=stable \
  -o APT::FTPArchive::Release::Components=main \
  -o "APT::FTPArchive::Release::Architectures=$(echo $ARCHES)" \
  release dists/stable > dists/stable/Release
gpg --batch --yes --local-user "$KEY" --clearsign -o dists/stable/InRelease dists/stable/Release
gpg --batch --yes --local-user "$KEY" -abs -o dists/stable/Release.gpg dists/stable/Release
gpg --batch --yes --export "$KEY" > cpuview.gpg
echo "APT repository in $OUT ($(echo $ARCHES))"
