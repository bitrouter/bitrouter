#!/usr/bin/env bash
set -Eeuo pipefail

exec socat \
    OPENSSL-LISTEN:8443,reuseaddr,fork,cert=/tls/server.crt,key=/tls/server.key,verify=0 \
    TCP:127.0.0.1:4358
