#!/bin/sh
# Download the Debian arm64 generic cloud image once. Needs curl.
# Latest tree ships .raw and .tar.xz, not .raw.xz.
set -e
root="$(cd "$(dirname "$0")/.." && pwd)"
outdir="${1:-$root/images}"
mkdir -p "$outdir"
# generic cloud image. It ships cloud-init, so a cidata disk is applied on boot.
url="https://cloud.debian.org/images/cloud/trixie/latest/debian-13-generic-arm64.tar.xz"
curl -fL "$url" -o "$outdir/debian.tar.xz"
rm -rf "$outdir/debian-unpack"
mkdir "$outdir/debian-unpack"
tar -xJf "$outdir/debian.tar.xz" -C "$outdir/debian-unpack"
raw=$(find "$outdir/debian-unpack" -name '*.raw' -print | head -n 1)
if [ -z "$raw" ]; then
    echo "no .raw inside $url" >&2
    exit 1
fi
mv "$raw" "$outdir/debian.raw"
rm -rf "$outdir/debian-unpack" "$outdir/debian.tar.xz"
echo "$outdir/debian.raw"
