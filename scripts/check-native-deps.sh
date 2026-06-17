#!/bin/sh
# check-native-deps.sh — verify TDLib's native runtime deps (OpenSSL 3 + zlib)
# are available for the current OS, and print the exact install command if not.
#
# Read-only: this script never installs anything and never uses sudo. It is the
# single per-OS source of truth shared by developers and CI. See
# docs/research/tdlib.md ("Native dependencies across targets") for why these
# deps exist (they are TDLib's, not removed by tdlib-rs's `static` feature).
#
# Exit codes: 0 = all deps satisfied, 1 = something missing (with guidance),
#             2 = unsupported OS.

set -eu

missing=0

note() { printf '  %s\n' "$1"; }
ok()   { printf 'OK   %s\n' "$1"; }
bad()  { printf 'MISS %s\n' "$1"; missing=1; }
warn() { printf 'WARN %s\n' "$1"; }   # suggested, not required — does not fail

uname_s=$(uname -s)
printf 'Checking TDLib native deps (OpenSSL 3 + zlib) for: %s\n\n' "$uname_s"

case "$uname_s" in
  Darwin)
    # OpenSSL on macOS is SUGGESTED, not mandatory: a plain download-tdlib build
    # loads openssl@3 from Homebrew at runtime, so local dev runs need it — but
    # release builds bundle it (scripts/bundle-native-deps.sh), so we never hard-
    # fail on it. Report present/absent without flipping the exit code.
    if command -v brew >/dev/null 2>&1 && brew --prefix openssl@3 >/dev/null 2>&1; then
      prefix=$(brew --prefix openssl@3)
      if [ -e "$prefix/lib/libssl.3.dylib" ] && [ -e "$prefix/lib/libcrypto.3.dylib" ]; then
        ok "OpenSSL 3 (Homebrew openssl@3 at $prefix)"
      else
        warn "openssl@3 keg present but libssl.3/libcrypto.3 missing at $prefix/lib"
      fi
    else
      warn "OpenSSL 3: Homebrew openssl@3 not found (suggested for local dev runs)"
      note "suggested: brew install openssl@3   (or: brew bundle)"
      note "not required for release builds — scripts/bundle-native-deps.sh ships OpenSSL with the app"
    fi
    # zlib: satisfied by the system (/usr/lib/libz), exposed via the SDK .tbd.
    if [ -e "$(xcrun --show-sdk-path 2>/dev/null)/usr/lib/libz.tbd" ]; then
      ok "zlib (system /usr/lib/libz via SDK)"
    else
      # The dylib lives in the dyld shared cache; absence of the .tbd only means
      # the CLT/SDK isn't set up, not that zlib is missing.
      note "zlib: system libz expected; ensure Xcode Command Line Tools are installed (xcode-select --install)"
    fi
    ;;

  Linux)
    # OpenSSL: need the runtime sonames libssl.so.3 / libcrypto.so.3.
    if ldconfig -p 2>/dev/null | grep -q 'libssl\.so\.3' \
       && ldconfig -p 2>/dev/null | grep -q 'libcrypto\.so\.3'; then
      ok "OpenSSL 3 (libssl.so.3 / libcrypto.so.3)"
    else
      bad "OpenSSL 3 (libssl.so.3 / libcrypto.so.3)"
    fi
    # zlib: need libz.so.1.
    if ldconfig -p 2>/dev/null | grep -q 'libz\.so\.1'; then
      ok "zlib (libz.so.1)"
    else
      bad "zlib (libz.so.1)"
    fi
    if [ "$missing" -ne 0 ]; then
      if command -v apt-get >/dev/null 2>&1; then
        note "install: sudo apt-get install -y libssl3 zlib1g"
      elif command -v dnf >/dev/null 2>&1; then
        note "install: sudo dnf install -y openssl-libs zlib"
      elif command -v pacman >/dev/null 2>&1; then
        note "install: sudo pacman -S --needed openssl zlib"
      else
        note "install your distro's OpenSSL 3 + zlib runtime packages"
      fi
    fi
    ;;

  MINGW*|MSYS*|CYGWIN*|Windows_NT)
    # The Windows prebuilt bundles its native deps; nothing to install.
    ok "Windows: OpenSSL/zlib are bundled in the prebuilt tdjson (nothing to install)"
    ;;

  *)
    printf 'Unsupported OS: %s\n' "$uname_s"
    exit 2
    ;;
esac

printf '\n'
if [ "$missing" -ne 0 ]; then
  printf 'Result: missing native deps — see install hint(s) above.\n'
  exit 1
fi
printf 'Result: all native deps satisfied.\n'
