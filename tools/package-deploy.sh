#!/usr/bin/env bash
# Package the server deploy kit attached to a release.
#
#   tools/package-deploy.sh 0.2.0 [outdir]
set -euo pipefail

VERSION=${1:?usage: package-deploy.sh <version> [outdir]}
OUT=${2:-dist}

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

mkdir -p "$OUT"
tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" "$NAME"
echo "==> $OUT/$NAME.tar.gz"
