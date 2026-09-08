#!/usr/bin/env bash
set -Eeuo pipefail

bitrouter context add administrator \
    --endpoint "${REMOTE_ADMIN_ENDPOINT:?remote endpoint is required}" \
    --token-env CLIENT_ADMIN_TOKEN >/tmp/context-administrator.json

exec python3 /usr/local/libexec/pty-check.py "$@"
