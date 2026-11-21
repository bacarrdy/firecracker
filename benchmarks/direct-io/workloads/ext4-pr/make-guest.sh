#!/usr/bin/env bash
set -euo pipefail
srcdir=$(cd "$(dirname "$0")" && pwd)
dest="$1"
mkdir -p "$dest"
temp=$(mktemp -d "$dest/build-XXXXXX")
trap 'rm -rf "$temp"' EXIT
mkdir -p "$temp"/{bin,dev,proc,sys}
install -m755 /usr/bin/busybox "$temp/bin/"
install -m755 "$srcdir/init" "$temp/init"
gcc -static -O2 -Wall -Wextra -Werror "$srcdir/probe.c" -o "$temp/bin/probe"
gcc -O2 -Wall -Wextra -Werror "$srcdir/dioalign.c" -o "$dest/dioalign"
(cd "$temp" && find . | cpio --quiet -o -H newc) | gzip -1 > "$dest/probe.gz"
