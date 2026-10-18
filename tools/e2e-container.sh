#!/usr/bin/env bash
# Functional test of the shipping pair: the server container and a client
# over the real wire.
#
#   cargo build --release -p parley-client
#   tools/e2e-container.sh
#
# Proves: the image builds, boots, bootstraps the admin account,
# `parley-server adduser` works inside the container, two accounts can
# authenticate and list channels, and a wrong password is refused.
# Needs Docker; no display, audio or video devices.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
# E2E_PORT, not PORT: an ambient PORT=0 in some environments (bundler
# sandboxes, dev shells) would otherwise silently publish a random port
# and the client would try to connect to :0.
IMAGE=${IMAGE:-parley:e2e}
E2E_PORT=${E2E_PORT:-44430}
CONTAINER="parley-e2e-$$"
CLIENT=${CLIENT:-"$ROOT/target/release/parley-client"}
ADMIN_PASS="e2e-admin-$$"
USER_PASS="e2e-user-$$"

WORK=$(mktemp -d)
cleanup() {
    docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT

if [ ! -x "$CLIENT" ]; then
    echo "client not built: $CLIENT" >&2
    echo "run: cargo build --release -p parley-client" >&2
    exit 1
fi

if [ "${BUILD:-1}" = "1" ]; then
    echo "==> image"
    docker build -t "$IMAGE" "$ROOT"
else
    echo "==> image $IMAGE (pre-built)"
fi

echo "==> boot"
docker run -d --name "$CONTAINER" \
    -p "127.0.0.1:${E2E_PORT}:4443/udp" \
    -e PARLEY_ADMIN_PASSWORD="$ADMIN_PASS" \
    "$IMAGE" >/dev/null

ready=0
for _ in $(seq 1 60); do
    if docker logs "$CONTAINER" 2>&1 | grep -q "parley listening"; then
        ready=1
        break
    fi
    sleep 0.5
done
if [ "$ready" -ne 1 ]; then
    echo "FAIL: the server never logged a listening line" >&2
    docker logs "$CONTAINER" >&2 || true
    exit 1
fi
docker logs "$CONTAINER" 2>&1 | grep -q "admin account created" || {
    echo "FAIL: the first-boot admin was not created" >&2
    docker logs "$CONTAINER" >&2
    exit 1
}
echo "    listening, admin bootstrapped"

echo "==> adduser inside the container"
created=0
for _ in $(seq 1 20); do
    if docker exec "$CONTAINER" /parley-server adduser alice "$USER_PASS" \
        >"$WORK/adduser.out" 2>"$WORK/adduser.err"; then
        created=1
        break
    fi
    sleep 0.5
done
if [ "$created" -ne 1 ]; then
    echo "FAIL: adduser did not succeed" >&2
    cat "$WORK/adduser.err" >&2
    exit 1
fi
cat "$WORK/adduser.out"

mkdir -p "$WORK/home"
check() {
    HOME="$WORK/home" "$CLIENT" \
        --server "127.0.0.1:${E2E_PORT}" --user "$1" --password "$2" --check
}

echo "==> admin login"
check admin "$ADMIN_PASS"
echo "==> added user login"
check alice "$USER_PASS"

echo "==> negative: wrong password"
if check admin "definitely-not-the-password" >"$WORK/negative.log" 2>&1; then
    echo "FAIL: the server accepted a wrong password" >&2
    cat "$WORK/negative.log" >&2
    exit 1
fi
grep -q "auth denied" "$WORK/negative.log" || {
    echo "FAIL: wrong password failed for the wrong reason" >&2
    cat "$WORK/negative.log" >&2
    exit 1
}
echo "    refused: $(grep -o 'auth denied.*' "$WORK/negative.log" | head -n1)"

# tracing writes ANSI around field NAMES even without a tty, so a
# literal "username=alice" never matches raw docker logs. Strip first.
server_logs=$(docker logs "$CONTAINER" 2>&1 | sed 's/\x1b\[[0-9;]*m//g')
if ! printf '%s\n' "$server_logs" | grep -q "session started username=alice"; then
    echo "FAIL: the server never logged alice's session" >&2
    printf '%s\n' "$server_logs" >&2
    exit 1
fi

echo "PASS: container + client end-to-end"
