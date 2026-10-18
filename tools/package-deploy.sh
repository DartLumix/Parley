#!/usr/bin/env bash
# Package the server deploy kit attached to a release.
#
#   tools/package-deploy.sh <version> [outdir] [image-tar]
#
# The kit is self-contained: the server's OCI image tarball ships INSIDE
# it, so deploying is one download, one extract and one `docker load` —
# never "install the compose file, then go find the matching image".
#
#   parley-deploy-<version>/
#     compose.yaml
#     .env.example
#     README.md
#     parley-image-<version>.tar.gz
#
# Without an image argument (a local kit, or building on a machine where
# `docker build` was skipped) the kit is still produced, with a warning.
# The release workflow always passes the release image.
set -euo pipefail

VERSION=${1:?usage: package-deploy.sh <version> [outdir] [image-tar]}
OUT=${2:-dist}
IMAGE=${3:-${PARLEY_IMAGE_TAR:-}}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
NAME="parley-deploy-${VERSION}"

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/$NAME"
cp "$ROOT/deploy/compose.yaml" "$STAGE/$NAME/compose.yaml"
cp "$ROOT/deploy/.env.example" "$STAGE/$NAME/.env.example"
cp "$ROOT/deploy/README.md" "$STAGE/$NAME/README.md"
# compose reads .env next to the compose file; the release ships an
# example, never a filled one.
sed -i "s/^PARLEY_IMAGE=.*/PARLEY_IMAGE=parley:${VERSION}/" \
    "$STAGE/$NAME/.env.example"
sed -i "s/^PARLEY_PORT=.*/PARLEY_PORT=4443/" "$STAGE/$NAME/.env.example"

if [ -n "$IMAGE" ] && [ -f "$IMAGE" ]; then
    cp "$IMAGE" "$STAGE/$NAME/parley-image-${VERSION}.tar.gz"
else
    echo "WARNING: no image tarball given — the kit cannot be deployed as-is." >&2
    echo "         Pass it as the third argument or in PARLEY_IMAGE_TAR." >&2
fi

mkdir -p "$OUT"
tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" "$NAME"
echo "==> $OUT/$NAME.tar.gz"
