# Native dependencies for tuigram on macOS (`brew bundle`).
#
# SUGGESTED for local dev, NOT required for end users. TDLib's prebuilt `tdjson`
# (via tdlib-rs `download-tdlib`) dynamically links OpenSSL 3 at an absolute
# Homebrew path — confirmed by `otool -L`:
#   /opt/homebrew/opt/openssl@3/lib/libssl.3.dylib  (+ libcrypto.3.dylib)
# so a plain dev build loads openssl@3 from here at runtime. Release builds
# instead bundle OpenSSL beside the binary (scripts/bundle-native-deps.sh), so
# distributed binaries need no Homebrew. zlib is the system lib, not needed here.
brew "openssl@3"

# zlib is intentionally absent: the prebuilt links the system /usr/lib/libz,
# so no Homebrew zlib is needed.

# Only needed for the from-source / pkg-config power-user build, not the
# default download-tdlib path. Harmless to install eagerly.
brew "pkg-config"
