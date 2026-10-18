#!/usr/bin/env bash
# Check a deployed server from any machine, using the released CLI.
#
#   tools/vps-check.sh parley.example.com:443 alice 'password'
#
# PARLEY_CLI points at the binary when it is not on PATH
# (default: parley-cli, then ./parley-cli, then target/release/parley-client).
set -euo pipefail

SERVER=${1:?usage: vps-check.sh <host[:port]> <user> <password>}
USER_NAME=${2:?usage: vps-check.sh <host[:port]> <user> <password>}
PASSWORD=${3:?usage: vps-check.sh <host[:port]> <user> <password>}

if [ -n "${PARLEY_CLI:-}" ]; then
    CLI=$PARLEY_CLI
elif command -v parley-cli >/dev/null 2>&1; then
    CLI=parley-cli
elif [ -x ./parley-cli ]; then
    CLI=./parley-cli
else
    CLI=target/release/parley-client
fi

exec "$CLI" --server "$SERVER" --user "$USER_NAME" --password "$PASSWORD" --check
