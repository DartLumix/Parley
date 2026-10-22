#!/usr/bin/env bash
# Build the terminal client used for diagnostics and the VPS smoke test.
#
#   tools/package-cli.sh 0.2.0 [outdir]
set -euo pipefail

VERSION=${1:?usage: package-cli.sh <version> [outdir]}
OUT=${2:-dist}

ROOT=$(cd "$(dirname "$0")/.." && pwd)
NAME="parley-cli-${VERSION}-linux-x86_64"

echo "==> terminal client (release, all features)"
(cd "$ROOT" && cargo build --release -p parley-client \
    --features audio-device,aec,video,screen-capture,vaapi)

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/$NAME"
cp "$ROOT/target/release/parley-client" "$STAGE/$NAME/parley-cli"
cp "$ROOT/LICENSE" "$STAGE/$NAME/LICENSE"
cat > "$STAGE/$NAME/README.txt" <<'EOF'
parley-cli — the terminal client and connection smoke test.

  parley-cli --server host[:port] --user USER --password PASS --check
      connect, authenticate, list channels, exit 0 (no audio/video needed)

  parley-cli --server host[:port] --user USER --password PASS
      join a voice channel and run the full client (mic, speakers, screen
      share with "share"/"stop" on stdin).

  --hw-encode / --dmabuf   VAAPI paths (only on machines that support them)
  --measure / --report     latency measurement (loud clicks)
  --share-audio            share the default output alongside the screen

Runtime: PipeWire and ALSA for the full client; --check needs neither.
EOF

mkdir -p "$OUT"
tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" "$NAME"
echo "==> $OUT/$NAME.tar.gz"
