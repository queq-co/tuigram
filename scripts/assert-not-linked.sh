#!/bin/sh
# assert-not-linked.sh — assert an artifact's dynamic dependency list does NOT
# contain any of the given needles (#167). The inverse of ci.yml's inline
# lib-level linkage audit, which asserts required deps ARE present on the
# dynamic libtdjson; this checks a static release *binary* carries no dynamic
# reference to what it statically linked in (tdjson).
#
#   Usage: scripts/assert-not-linked.sh <linux|macos> <path-to-artifact> <needle> [needle...]
#
# Prints the full dependency list for the record (matching the existing
# lib-level audit's style), then fails loudly if any needle appears.

set -eu

os=${1:-}
artifact=${2:-}
shift 2 2>/dev/null || { printf 'usage: %s <linux|macos> <path-to-artifact> <needle...>\n' "$0" >&2; exit 64; }
[ "$#" -ge 1 ] || { printf 'usage: %s <linux|macos> <path-to-artifact> <needle...>\n' "$0" >&2; exit 64; }

[ -f "$artifact" ] || { printf 'FAIL: no artifact at %s\n' "$artifact" >&2; exit 1; }

case "$os" in
  linux)
    echo "== ldd $artifact =="
    deps=$(ldd "$artifact")
    ;;
  macos)
    echo "== otool -L $artifact =="
    deps=$(otool -L "$artifact")
    ;;
  *)
    printf 'unsupported os: %s (expected linux or macos)\n' "$os" >&2
    exit 64
    ;;
esac
echo "$deps"

fail=0
for needle in "$@"; do
  if printf '%s' "$deps" | grep -qi "$needle"; then
    printf 'FAIL: %s unexpectedly linked in %s\n' "$needle" "$artifact" >&2
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  exit 1
fi

printf 'OK: %s carries no dynamic reference to: %s\n' "$artifact" "$*"
