#!/bin/sh
# Set up vendor/smithay/ with the OpenBSD patches applied. The vendored
# tree is pointed at from Cargo.toml's `[patch.crates-io]`, so it must
# exist before `cargo build`. Re-running this script is safe — it
# recreates vendor/smithay/ from scratch.
#
# Prefers the local cargo registry cache (populated on first `cargo
# fetch`); falls back to downloading the crate from crates.io.
set -eu

VERSION=0.7.0
REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
PATCH_DIR=$REPO_ROOT/contrib/openbsd
VENDOR_DIR=$REPO_ROOT/vendor/smithay

REGISTRY_SRC=$(find "${CARGO_HOME:-$HOME/.cargo}/registry/src" \
    -maxdepth 2 -type d -name "smithay-$VERSION" 2>/dev/null | head -n1)

mkdir -p "$(dirname "$VENDOR_DIR")"
rm -rf "$VENDOR_DIR"

if [ -n "$REGISTRY_SRC" ] && [ -d "$REGISTRY_SRC" ]; then
    echo "vendoring smithay $VERSION from $REGISTRY_SRC"
    cp -R "$REGISTRY_SRC" "$VENDOR_DIR"
else
    echo "smithay $VERSION not in the cargo registry cache; downloading"
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    url="https://crates.io/api/v1/crates/smithay/$VERSION/download"
    ftp -o "$tmp/smithay.tar.gz" "$url" 2>/dev/null \
        || curl -fsSL "$url" -o "$tmp/smithay.tar.gz"
    (cd "$tmp" && tar -xzf smithay.tar.gz)
    mv "$tmp/smithay-$VERSION" "$VENDOR_DIR"
fi

# The registry's normalized Cargo.toml is what cargo actually resolves
# against; leave it in place. (Cargo.toml.orig uses `path =` workspace
# members that aren't in the published tarball.)

for p in "$PATCH_DIR"/*.patch; do
    echo "applying $(basename "$p")"
    (cd "$VENDOR_DIR" && patch -p1 --silent < "$p")
done
find "$VENDOR_DIR" -name '*.orig' -delete

echo "done — vendor/smithay/ is ready at $VENDOR_DIR"
