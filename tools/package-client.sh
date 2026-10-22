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
    --features audio-device,aec,video,screen-capture,hw-encode)

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

# Portability. If this machine has a system abseil, the WebRTC AEC links
# those shared libraries instead of the static copy. Ship them INSIDE the
# tarball's lib/ (the bridge carries a $ORIGIN runpath for exactly this,
# see app/rust/build.rs) so the build does not depend on the target
# machine having the same abseil installed.
BRIDGE="$STAGE/$NAME/lib/libparley_flutter.so"
bundled=0
while IFS= read -r lib; do
    [ -n "$lib" ] || continue
    cp -L "$lib" "$STAGE/$NAME/lib/" && bundled=$((bundled + 1))
done < <(ldd "$BRIDGE" 2>/dev/null |
    awk '/=> \// {print $3}' |
    grep -E 'lib(absl|webrtc)' || true)
if [ "$bundled" -gt 0 ]; then
    echo "==> bundled $bundled system WebRTC/abseil libraries into lib/"
fi
# The bundled copies are only found if the bridge looks in its own
# directory; assert that instead of trusting it silently.
if [ "$bundled" -gt 0 ] &&
    ! readelf -d "$BRIDGE" | grep -q 'RUNPATH.*\$ORIGIN'; then
    echo "ERROR: external libraries were bundled but the bridge has no" >&2
    echo "       \$ORIGIN runpath, so it would still load the host's copies." >&2
    echo "       (app/rust/build.rs sets it; a stale bridge .so is the usual cause.)" >&2
    exit 1
fi

mkdir -p "$OUT"
tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" "$NAME"
echo "==> $OUT/$NAME.tar.gz"
