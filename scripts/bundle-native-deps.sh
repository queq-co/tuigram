#!/bin/sh
# bundle-native-deps.sh — make a built tuigram binary self-contained w.r.t.
# TDLib's native deps, natively and per-OS, with NO package manager required at
# end-user runtime. See docs/research/tdlib.md ("Distribution strategy").
#
#   Usage: scripts/bundle-native-deps.sh <path-to-binary> [openssl-prefix]
#
# - macOS: copies the OpenSSL 3 dylibs the binary references (from the build
#   host's openssl@3) next to the binary in ./lib, rewrites the Mach-O load
#   commands to @loader_path via install_name_tool, ad-hoc re-codesigns, and
#   asserts no absolute Homebrew/openssl paths remain. zlib stays the system
#   /usr/lib/libz (already relative-safe), so it is left untouched.
# - Linux / Windows: no-op by design — Linux uses the system OpenSSL/zlib
#   (declared as package deps) and the Windows prebuilt already bundles them.
#
# Idempotent: re-running on an already-bundled binary is a no-op for paths that
# are already @loader_path/@rpath.

set -eu

uname_s=$(uname -s)

case "$uname_s" in
  Linux)
    printf 'Linux: no bundling needed — depends on system libssl.so.3 / libz.so.1 '
    printf '(declare as package deps). Nothing to do.\n'
    exit 0
    ;;
  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    printf 'Windows: OpenSSL/zlib are bundled in the prebuilt tdjson. Nothing to do.\n'
    exit 0
    ;;
  Darwin) : ;;  # fall through to the real work
  *)
    printf 'Unsupported OS: %s\n' "$uname_s"; exit 2 ;;
esac

# ---- macOS ----------------------------------------------------------------
bin=${1:-}
if [ -z "$bin" ] || [ ! -f "$bin" ]; then
  printf 'usage: %s <path-to-binary> [openssl-prefix]\n' "$0" >&2
  exit 64
fi

bindir=$(cd "$(dirname "$bin")" && pwd)
libdir="$bindir/lib"

# OpenSSL source: explicit arg, else the build host's Homebrew keg.
ossl_prefix=${2:-}
if [ -z "$ossl_prefix" ]; then
  if command -v brew >/dev/null 2>&1 && brew --prefix openssl@3 >/dev/null 2>&1; then
    ossl_prefix=$(brew --prefix openssl@3)
  else
    printf 'error: no openssl@3 source — pass an [openssl-prefix] or install it on the build host\n' >&2
    exit 1
  fi
fi

# Discover the absolute openssl dylib paths the binary references.
ossl_refs=$(otool -L "$bin" | awk '/openssl@3.*\.dylib/ {print $1}')
if [ -z "$ossl_refs" ]; then
  printf 'No absolute openssl@3 references in %s — already bundled or statically resolved. Done.\n' "$bin"
  exit 0
fi

mkdir -p "$libdir"
printf 'Bundling OpenSSL into %s (from %s)\n' "$libdir" "$ossl_prefix"

# Copy each referenced dylib in, normalize its own id to @rpath, and rewrite the
# binary's reference to @loader_path/lib/<name>.
for ref in $ossl_refs; do
  name=$(basename "$ref")
  src="$ossl_prefix/lib/$name"
  [ -f "$src" ] || { printf 'error: %s not found\n' "$src" >&2; exit 1; }
  cp -f "$src" "$libdir/$name"
  chmod u+w "$libdir/$name"
  install_name_tool -id "@rpath/$name" "$libdir/$name"
  install_name_tool -change "$ref" "@loader_path/lib/$name" "$bin"
done

# Fix inter-dylib references (libssl depends on libcrypto by absolute path).
for dy in "$libdir"/*.dylib; do
  for dep in $(otool -L "$dy" | awk '/openssl@3.*\.dylib/ {print $1}'); do
    install_name_tool -change "$dep" "@rpath/$(basename "$dep")" "$dy"
  done
done

# The dylibs are found relative to the binary.
install_name_tool -add_rpath "@loader_path/lib" "$bin" 2>/dev/null || true

# Changing load commands invalidates code signatures on Apple Silicon — re-sign
# ad-hoc (release pipelines replace this with a real identity).
codesign --force --sign - "$libdir"/*.dylib "$bin" 2>/dev/null || true

# Assert no absolute Homebrew/openssl paths survive in the shipped binary.
if otool -L "$bin" | grep -Eq '/(opt/homebrew|usr/local)/.*openssl'; then
  printf 'FAIL: absolute openssl paths still present in %s:\n' "$bin" >&2
  otool -L "$bin" | grep -E 'openssl|@(loader_path|rpath)' >&2
  exit 1
fi

printf 'OK: %s is self-contained for OpenSSL (no Homebrew required at runtime).\n' "$bin"
otool -L "$bin" | grep -Ei 'ssl|crypto|libz|@(loader_path|rpath)' || true
