#!/usr/bin/env bash
set -Eeuo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
readonly script_dir
repository_root=$(cd -- "$script_dir/../.." && pwd)
readonly repository_root
readonly image=${BITROUTER_REMOTE_ADMIN_IMAGE:-bitrouter-remote-administration:local}
readonly run_id="bitrouter-remote-admin-${RANDOM}-$$"
readonly network_name="${run_id}-net"
readonly server_name="${run_id}-server"
readonly proxy_name="${run_id}-proxy"
readonly partial_server_name="${run_id}-partial-server"
readonly partial_proxy_name="${run_id}-partial-proxy"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/bitrouter-remote-administration.XXXXXX")
readonly scratch
readonly server_dir="$scratch/server"
readonly tls_dir="$scratch/tls"

# These are fixture-only bearer values. Config references their variable names;
# values live only in the respective ephemeral container environments.
readonly reader_token='reader-control-token-012345678901234567890123456789'
readonly admin_token='admin-control-token-0123456789012345678901234567890'
readonly upstream_token='fixture-upstream-token-012345678901234567890123456'

cleanup() {
    local status=$?
    if [[ "$status" -ne 0 ]]; then
        printf '%s\n' 'remote-administration container logs follow:' >&2
        docker logs "$partial_proxy_name" >&2 2>/dev/null || true
        docker logs "$partial_server_name" >&2 2>/dev/null || true
        docker logs "$proxy_name" >&2 2>/dev/null || true
        docker logs "$server_name" >&2 2>/dev/null || true
    fi
    docker rm --force \
        "$partial_proxy_name" \
        "$partial_server_name" \
        "$proxy_name" \
        "$server_name" >/dev/null 2>&1 || true
    docker network rm "$network_name" >/dev/null 2>&1 || true
    rm -rf -- "$scratch"
    exit "$status"
}
trap cleanup EXIT

write_policy() {
    local model=$1
    printf '%s\n' \
        'lockfileVersion: 1' \
        'policies:' \
        '  fixture:' \
        '    key_strategy: agent_trace' \
        '    tiers:' \
        "      chosen: fixture:$model" \
        '    routes: {}' \
        '    default_tier: chosen' \
        '    tool_use_tier: chosen' \
        '    tool_safe_tiers: [chosen]' \
        '  fixture-detail:' \
        '    key_strategy: agent_trace' \
        '    tiers:' \
        '      concise: fixture:fixture-a' \
        '      detailed: fixture:fixture-b' \
        '    routes: {}' \
        '    default_tier: concise' \
        '    tool_use_tier: detailed' \
        '    tool_safe_tiers: [concise, detailed]' \
        >"$server_dir/policy-lock.yaml"
}

write_server_config() {
    install -d -m 0700 "$server_dir" "$tls_dir"
    printf '%s\n' \
        'server:' \
        '  listen: "0.0.0.0:4356"' \
        '  control_socket: "/tmp/bitrouter.sock"' \
        '  skip_auth: true' \
        'database:' \
        '  url: "sqlite:///tmp/bitrouter.db?mode=rwc"' \
        'control:' \
        '  enabled: true' \
        '  listen: "127.0.0.1:4358"' \
        '  credentials:' \
        '    - id: reader' \
        '      token_env: SERVER_READER_TOKEN' \
        '      scopes: ["control:read"]' \
        '    - id: administrator' \
        '      token_env: SERVER_ADMIN_TOKEN' \
        '      scopes: ["control:read", "control:reload"]' \
        'inherit_defaults: false' \
        'registry:' \
        '  enabled: false' \
        'providers:' \
        '  fixture:' \
        '    api_base: "http://127.0.0.1:8080/v1"' \
        "    api_key: \"\${SERVER_UPSTREAM_TOKEN}\"" \
        '    api_protocol:' \
        '      - "*": chat_completions' \
        '    models:' \
        '      - id: fixture-a' \
        '      - id: fixture-b' \
        'policy:' \
        '  path: "./policy-lock.yaml"' \
        '  mode: frozen' \
        'presets:' \
        '  fixture:' \
        '    model: fixture:fixture-a' \
        '    policy: fixture' \
        >"$server_dir/bitrouter.yaml"
    write_policy fixture-a
}

write_tls_material() {
    openssl req \
        -x509 \
        -newkey rsa:2048 \
        -nodes \
        -days 1 \
        -subj '/CN=bitrouter-remote-administration test CA' \
        -addext 'basicConstraints=critical,CA:TRUE' \
        -addext 'keyUsage=critical,keyCertSign,cRLSign' \
        -keyout "$tls_dir/ca.key" \
        -out "$tls_dir/ca.crt" >/dev/null 2>&1
    openssl req \
        -newkey rsa:2048 \
        -nodes \
        -subj '/CN=server' \
        -keyout "$tls_dir/server.key" \
        -out "$tls_dir/server.csr" >/dev/null 2>&1
    printf '%s\n' \
        'basicConstraints=critical,CA:FALSE' \
        'keyUsage=critical,digitalSignature,keyEncipherment' \
        'extendedKeyUsage=serverAuth' \
        'subjectAltName=DNS:server,DNS:localhost' \
        >"$tls_dir/server.ext"
    openssl x509 \
        -req \
        -in "$tls_dir/server.csr" \
        -CA "$tls_dir/ca.crt" \
        -CAkey "$tls_dir/ca.key" \
        -CAcreateserial \
        -days 1 \
        -sha256 \
        -extfile "$tls_dir/server.ext" \
        -out "$tls_dir/server.crt" >/dev/null 2>&1
    rm -f -- "$tls_dir/ca.key" "$tls_dir/ca.srl" "$tls_dir/server.csr" "$tls_dir/server.ext"
}

build_image() {
    if [[ "${BITROUTER_REMOTE_ADMIN_SKIP_BUILD:-0}" == 1 ]]; then
        return
    fi
    DOCKER_BUILDKIT=1 docker build \
        --platform linux/arm64 \
        --progress=plain \
        --file "$script_dir/Dockerfile" \
        --tag "$image" \
        "$repository_root"
}

start_server() {
    docker network create "$network_name" >/dev/null
    docker run \
        --detach \
        --name "$server_name" \
        --network "$network_name" \
        --network-alias server \
        --volume "$server_dir:/work/server" \
        --env "SERVER_READER_TOKEN=$reader_token" \
        --env "SERVER_ADMIN_TOKEN=$admin_token" \
        --env "BITROUTER_CONTROL_TOKEN=$reader_token" \
        --env "SERVER_UPSTREAM_TOKEN=$upstream_token" \
        "$image" \
        /usr/local/bin/remote-admin-server >/dev/null
    start_proxy "$server_name" "$proxy_name"
}

start_proxy() {
    local target_server=$1
    local target_proxy=$2
    docker run \
        --detach \
        --name "$target_proxy" \
        --network "container:$target_server" \
        --volume "$tls_dir:/tls:ro" \
        "$image" \
        /usr/local/bin/remote-admin-tls-proxy >/dev/null
}

wait_for_control() {
    for _ in $(seq 1 120); do
        if docker run \
            --rm \
            --network "$network_name" \
            --volume "$tls_dir/ca.crt:/tls/ca.crt:ro" \
            "$image" \
            curl \
                --silent \
                --show-error \
                --fail \
                --cacert /tls/ca.crt \
                --header "Authorization: Bearer $reader_token" \
                https://server:8443/control/v1/capabilities >/dev/null 2>&1; then
            return
        fi
        sleep 0.25
    done
    printf '%s\n' 'control API did not become ready' >&2
    return 1
}

run_client() {
    local phase=$1
    shift
    local -a env_args=(
        --env HOME=/home/bitrouter
        --env BITROUTER_HOME=/home/bitrouter/.bitrouter
        --env SSL_CERT_FILE=/tls/ca.crt
        --env "CLIENT_READ_TOKEN=$reader_token"
    )
    if [[ "$phase" == reload || "$phase" == cli-reload || "$phase" == boot-change || "$phase" == legacy || "$phase" == partial ]]; then
        env_args+=(--env "CLIENT_ADMIN_TOKEN=$admin_token")
    fi
    # The default acceptance path always invokes MCP. This is a diagnostic
    # selector for completing independent live journeys after an MCP failure.
    if [[ "${BITROUTER_REMOTE_ADMIN_SKIP_MCP:-0}" == "1" ]]; then
        env_args+=(--env BITROUTER_REMOTE_ADMIN_SKIP_MCP=1)
    fi
    docker run \
        --rm \
        --network "$network_name" \
        --tmpfs /home/bitrouter:rw,noexec,nosuid,size=16m \
        --volume "$tls_dir/ca.crt:/tls/ca.crt:ro" \
        "${env_args[@]}" \
        "$image" \
        /usr/local/bin/remote-admin-client \
        /usr/local/bin/remote-admin-checks \
        "$phase" \
        "$@"
}

run_pty_check() {
    local expected=$1
    local -a pty_args=(
        /usr/local/bin/remote-admin-client
        /usr/local/bin/remote-admin-pty
        --context administrator
        --expected "$expected"
        --token-env CLIENT_ADMIN_TOKEN
        --endpoint https://server:8443/control/v1
        --ca /tls/ca.crt
    )
    if [[ "$expected" == succeeded ]]; then
        pty_args+=(--read-panels)
    fi
    docker run \
        --rm \
        --network "$network_name" \
        --tmpfs /home/bitrouter:rw,noexec,nosuid,size=16m \
        --volume "$tls_dir/ca.crt:/tls/ca.crt:ro" \
        --env HOME=/home/bitrouter \
        --env BITROUTER_HOME=/home/bitrouter/.bitrouter \
        --env SSL_CERT_FILE=/tls/ca.crt \
        --env REMOTE_ADMIN_ENDPOINT=https://server:8443/control/v1 \
        --env "CLIENT_READ_TOKEN=$reader_token" \
        --env "CLIENT_ADMIN_TOKEN=$admin_token" \
        "$image" \
        "${pty_args[@]}"
}

restart_server() {
    docker rm --force "$proxy_name" >/dev/null
    docker restart "$server_name" >/dev/null
    start_proxy "$server_name" "$proxy_name"
    wait_for_control
}

remove_explicit_credentials() {
    python3 - "$server_dir/bitrouter.yaml" <<'PYTHON'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
start = text.find("  credentials:\n")
end = text.find("inherit_defaults: false\n", start)
if start < 0 or end < 0:
    raise SystemExit("fixture credentials block was not found")
path.write_text(text[:start] + text[end:])
PYTHON
}

stop_production_server() {
    docker rm --force "$proxy_name" "$server_name" >/dev/null
}

start_partial_fixture() {
    docker run \
        --detach \
        --name "$partial_server_name" \
        --network "$network_name" \
        --network-alias server \
        --env "SERVER_READER_TOKEN=$reader_token" \
        --env "SERVER_ADMIN_TOKEN=$admin_token" \
        "$image" \
        /usr/local/bin/bitrouter-lib-tests \
        remote_control::tests::docker_partial_fixture \
        --ignored \
        --exact \
        --nocapture >/dev/null
    start_proxy "$partial_server_name" "$partial_proxy_name"
    wait_for_control
}

main() {
    build_image
    write_server_config
    write_tls_material
    start_server
    wait_for_control

    run_client read

    # The host stages this file change. The next reader observes disk and live
    # policy disagreeing before an administrator chooses to apply it.
    write_policy fixture-b
    run_client diverged
    run_client cli-reload

    local reload_output
    reload_output=$(run_client reload)
    printf '%s\n' "$reload_output"
    local request_id
    request_id=$(awk -F= '$1 == "REMOTE_ADMIN_OPERATION_ID" { print $2 }' <<<"$reload_output")
    local prior_instance
    prior_instance=$(awk -F= '$1 == "REMOTE_ADMIN_OPERATION_INSTANCE" { print $2 }' <<<"$reload_output")
    [[ -n "$request_id" && -n "$prior_instance" ]] \
        || fail 'reload client did not return its recoverable operation identity'

    restart_server
    run_client boot-change "$request_id" "$prior_instance"
    run_pty_check succeeded

    # Credentials are intentionally removed only after the scoped production
    # journey. The legacy token is then the sole accepted reader credential.
    remove_explicit_credentials
    restart_server
    run_client legacy

    stop_production_server
    start_partial_fixture
    run_client partial
    run_pty_check partially_applied

    if [[ "${BITROUTER_REMOTE_ADMIN_SKIP_MCP:-0}" == "1" ]]; then
        printf '%s\n' 'remote-administration selected CLI/Code acceptance passed (MCP skipped)'
    else
        printf '%s\n' 'remote-administration Docker acceptance passed'
    fi
}

fail() {
    printf 'remote-administration Docker acceptance failed: %s\n' "$*" >&2
    return 1
}

main "$@"
