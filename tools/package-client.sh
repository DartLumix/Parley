#!/usr/bin/env bash
# Build the distributable Linux client tarball.
#
#   tools/package-client.sh 0.2.0 [outdir]
#
# Runs both halves in release mode (a debug bridge in a release bundle is
# the trap documented in docs/two-machine-demo.md), then stages:
#
#   parley-<version>-linux-x86_64/
#     parley, lib/, data/     — the Flutter bundle
#     install.sh              — per-user install, dependency check
#     parley.svg, QUICKSTART.txt, LICENSE
set -euo pipefail

VERSION=${1:?usage: package-client.sh <version> [outdir]}
OUT=${2:-dist}
BUILD_NUMBER=${BUILD_NUMBER:-1}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
NAME="parley-${VERSION}-linux-x86_64"
BUNDLE="$ROOT/app/build/linux/x64/release/bundle"

echo "==> bridge (release, all features)"
(cd "$ROOT" && cargo build --release -p parley_flutter \
    --features audio-device,video,screen-capture,hw-encode)

echo "==> Flutter bundle"
(cd "$ROOT/app" && flutter build linux --release \
    --build-name "$VERSION" \
    --build-number "$BUILD_NUMBER" \
    --dart-define=PARLEY_VERSION="$VERSION")

test -x "$BUNDLE/parley" || { echo "no bundle at $BUNDLE" >&2; exit 1; }
test -f "$BUNDLE/lib/libparley_flutter.so" || {
    echo "the bridge .so is missing from the bundle" >&2
    exit 1
}

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/$NAME"
cp -R "$BUNDLE"/. "$STAGE/$NAME"/
cp "$ROOT/packaging/install.sh" "$STAGE/$NAME/install.sh"
cp "$ROOT/packaging/parley.svg" "$STAGE/$NAME/parley.svg"
cp "$ROOT/packaging/QUICKSTART.txt" "$STAGE/$NAME/QUICKSTART.txt"
cp "$ROOT/LICENSE" "$STAGE/$NAME/LICENSE"
chmod +x "$STAGE/$NAME/install.sh" "$STAGE/$NAME/parley"

# Portability guard. The AEC is statically linked, but if this machine has
# a system abseil >= 20240722 the bundled WebRTC build links that instead —
# the tarball then only runs on machines with the same abseil. The release
# CI has no system abseil, so its builds are self-contained.
system_linkage=$(ldd "$STAGE/$NAME/lib/libparley_flutter.so" 2>/dev/null |
    grep -E "webrtc-audio|absl" || true)
if [ -n "$system_linkage" ]; then
    echo "WARNING: this build links system WebRTC/abseil libraries:" >&2
    echo "$system_linkage" >&2
    echo "         it will not run on a machine without them." >&2
    echo "         Release builds run in CI, which has neither." >&2
fi

mkdir -p "$OUT"
tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" "$NAME"
echo "==> $OUT/$NAME.tar.gz"
