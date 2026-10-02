#!/usr/bin/env bash
set -Eeuo pipefail

# Resolve the policy lock from the mounted server configuration directory.
# The fixture explicitly keeps its daemon socket and SQLite file container-local.
cd /work/server

python3 /usr/local/libexec/mock-openai.py &
mock_pid=$!
trap 'kill "$mock_pid" 2>/dev/null || true' EXIT

for _ in $(seq 1 40); do
    if curl --silent --fail http://127.0.0.1:8080/health >/dev/null; then
        exec bro serve --config /work/server/bitrouter.yaml
    fi
    sleep 0.1
done

printf '%s\n' 'mock upstream did not start' >&2
exit 1
