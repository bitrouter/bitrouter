#!/usr/bin/env bash
set -Eeuo pipefail

# The Rust client uses the system trust store while curl gets --cacert below.
# This copy is made inside the ephemeral client container, never in its home.
install -D -m 0644 /tls/ca.crt /usr/local/share/ca-certificates/remote-admin-ca.crt
update-ca-certificates >/dev/null

exec "$@"
