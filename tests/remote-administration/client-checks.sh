#!/usr/bin/env bash
set -Eeuo pipefail

readonly control_endpoint="https://server:8443/control/v1"
readonly ca_file="/tls/ca.crt"

fail() {
    printf 'remote-administration acceptance failed: %s\n' "$*" >&2
    exit 1
}

assert_json() {
    local value=$1
    local expression=$2
    jq --exit-status "$expression" <<<"$value" >/dev/null \
        || fail "JSON assertion failed: $expression"
}

utc_timestamp() {
    python3 - "${1:-0}" <<'PYTHON'
from datetime import datetime, timedelta, timezone
import sys

value = datetime.now(timezone.utc) + timedelta(seconds=int(sys.argv[1]))
print(value.isoformat(timespec="microseconds").replace("+00:00", "Z"))
PYTHON
}

assert_client_boundary() {
    [[ ! -e "$HOME/bitrouter.yaml" ]] || fail 'client home unexpectedly contains a router config'
    [[ ! -e "$BITROUTER_HOME/bitrouter.yaml" ]] \
        || fail 'client BitRouter home unexpectedly contains a router config'
    [[ ! -e /tls/server.key && ! -e /tls/server.crt ]] \
        || fail 'client unexpectedly received TLS private-key material'
    for name in \
        ANTHROPIC_API_KEY \
        GEMINI_API_KEY \
        OPENAI_API_KEY \
        BITROUTER_API_KEY \
        SERVER_UPSTREAM_TOKEN \
        TEST_UPSTREAM_TOKEN; do
        if printenv "$name" >/dev/null; then
            fail "client unexpectedly received provider credential variable $name"
        fi
    done
}

control_get() {
    local token=$1
    local path=$2
    shift 2
    local -a args=(
        --silent
        --show-error
        --fail-with-body
        --cacert "$ca_file"
        --header "Authorization: Bearer $token"
        --header 'Accept: application/json'
        --get
    )
    local query
    for query in "$@"; do
        args+=(--data-urlencode "$query")
    done
    curl "${args[@]}" "$control_endpoint$path"
}

control_post() {
    local token=$1
    local path=$2
    local body=$3
    curl \
        --silent \
        --show-error \
        --fail-with-body \
        --cacert "$ca_file" \
        --header "Authorization: Bearer $token" \
        --header 'Content-Type: application/json' \
        --data "$body" \
        "$control_endpoint$path"
}

control_status() {
    local token=$1
    local method=$2
    local path=$3
    local expected_status=$4
    local body=${5-}
    local output
    output=$(mktemp)
    local code
    local -a args=(
        --silent
        --show-error
        --cacert "$ca_file"
        --header "Authorization: Bearer $token"
        --request "$method"
        --output "$output"
        --write-out '%{http_code}'
    )
    if [[ -n "$body" ]]; then
        args+=(--header 'Content-Type: application/json' --data "$body")
    fi
    code=$(curl "${args[@]}" "$control_endpoint$path")
    if [[ "$code" != "$expected_status" ]]; then
        local response
        response=$(<"$output")
        rm -f "$output"
        fail "expected HTTP $expected_status from $method $path, got $code: $response"
    fi
    local response
    response=$(<"$output")
    rm -f "$output"
    printf '%s' "$response"
}

add_reader_context() {
    bitrouter context add reader \
        --endpoint "$control_endpoint" \
        --token-env CLIENT_READ_TOKEN >/tmp/context-reader.json
}

assert_cli_reaches_tls_proxy() {
    add_reader_context
    bitrouter --context reader status >/tmp/status.json
    bitrouter --context reader models >/tmp/models.json
    jq --exit-status 'has("running")' /tmp/status.json >/dev/null \
        || fail 'remote status CLI response is not a status report'
    jq --exit-status 'has("models")' /tmp/models.json >/dev/null \
        || fail 'remote models CLI response is not a models report'
}

assert_cli_read_actions() {
    local since
    since=$(utc_timestamp -600)
    local until
    until=$(utc_timestamp)

    bitrouter --context reader status >/tmp/cli-status.json
    bitrouter --context reader status --requests >/tmp/cli-status-requests.json
    bitrouter --context reader models --provider fixture >/tmp/cli-models.json
    bitrouter --context reader requests \
        --limit 1 \
        --model fixture-a \
        --provider fixture \
        --since "$since" \
        --until "$until" >/tmp/cli-requests.json
    bitrouter --context reader route fixture-a \
        --prompt 'container acceptance' >/tmp/cli-route.json
    bitrouter --context reader providers list >/tmp/cli-providers.json
    bitrouter --context reader observe status >/tmp/cli-observe.json
    bitrouter --context reader policy status --view active >/tmp/cli-policy-status.json
    bitrouter --context reader policy show fixture \
        --view active >/tmp/cli-policy-show.json
    bitrouter --context reader agents list >/tmp/cli-agents.json

    jq --exit-status 'has("running")' /tmp/cli-status.json >/dev/null \
        || fail 'remote status CLI did not return a status report'
    jq --exit-status '.rows | type == "array"' /tmp/cli-status-requests.json >/dev/null \
        || fail 'remote status --requests CLI did not return a requests report'
    jq --exit-status '.models | type == "array"' /tmp/cli-models.json >/dev/null \
        || fail 'remote models CLI did not return a models report'
    jq --exit-status '
        .filters.model == "fixture-a"
        and .filters.provider == "fixture"
        and (.rows | length) == 1
        and .truncated == true
    ' /tmp/cli-requests.json >/dev/null \
        || fail 'remote requests CLI did not preserve request filters'
    local expected_since=${since/Z/+00:00}
    local expected_until=${until/Z/+00:00}
    [[ "$(jq -r '.filters.since' /tmp/cli-requests.json)" == "$expected_since" ]] \
        || fail 'remote requests CLI did not preserve the inclusive request bound'
    [[ "$(jq -r '.filters.until' /tmp/cli-requests.json)" == "$expected_until" ]] \
        || fail 'remote requests CLI did not preserve the exclusive request bound'
    printf 'REMOTE_ADMIN_CLI_REQUEST_FILTER_BOUNDS since=%s until=%s newest_row=%s\n' \
        "$expected_since" \
        "$expected_until" \
        "$(jq -r '.rows[0].created_at' /tmp/cli-requests.json)"
    jq --exit-status '.requested_model == "fixture-a"' /tmp/cli-route.json >/dev/null \
        || fail 'remote route CLI did not return a route report'
    jq --exit-status '.providers | type == "array"' /tmp/cli-providers.json >/dev/null \
        || fail 'remote providers CLI did not return a providers report'
    jq --exit-status 'has("daemon_reachable")' /tmp/cli-observe.json >/dev/null \
        || fail 'remote observe CLI did not return an observation report'
    jq --exit-status '.view == "active" and .availability == "available"' \
        /tmp/cli-policy-status.json >/dev/null \
        || fail 'remote policy status CLI did not use the active view'
    jq --exit-status '.view == "active" and .definitions.fixture != null' \
        /tmp/cli-policy-show.json >/dev/null \
        || fail 'remote policy show CLI did not use the active view'
    jq --exit-status '.agents | type == "array"' /tmp/cli-agents.json >/dev/null \
        || fail 'remote agents CLI did not return an agents report'
}

assert_mcp_control_tools() {
    local report
    report=$(python3 /usr/local/libexec/mcp-check.py \
        --endpoint 'https://server:8443/mcp-control' \
        --ca "$ca_file" \
        --token-env CLIENT_READ_TOKEN)
    assert_json "$report" '
        .mcp == "passed"
        and .tools == ["list_models", "route_preview", "status"]
    '
}

assert_remote_target_isolation() {
    local report
    report=$(python3 /usr/local/libexec/target-isolation.py \
        --binary bitrouter \
        --endpoint "$control_endpoint" \
        --ca "$ca_file")
    assert_json "$report" '
        .target_isolation == "passed"
        and .read_actions == 9
        and .failure_modes == 2
    '
}

assert_control_is_loopback_only() {
    if curl \
        --silent \
        --show-error \
        --connect-timeout 1 \
        --max-time 1 \
        http://server:4358/control/v1/capabilities >/dev/null 2>&1; then
        fail 'client reached the control listener without the TLS proxy'
    fi
}

seed_requests() {
    local model
    for model in fixture-a fixture-a fixture-a fixture-b; do
        curl \
            --silent \
            --show-error \
            --fail-with-body \
            --header 'Content-Type: application/json' \
            --data "{\"model\":\"$model\",\"messages\":[{\"role\":\"user\",\"content\":\"container acceptance\"}]}" \
            http://server:4356/v1/chat/completions >/dev/null
    done

    if curl \
        --silent \
        --show-error \
        --fail-with-body \
        --header 'Content-Type: application/json' \
        --data '{"model":"fixture-a","messages":[{"role":"user","content":"force-rate-limit"}]}' \
        http://server:4356/v1/chat/completions >/dev/null; then
        fail 'fixture upstream was expected to reject the synthetic rate-limited request'
    fi
}

wait_for_metering() {
    local report
    local count
    for _ in $(seq 1 80); do
        report=$(control_get "$CLIENT_READ_TOKEN" '/requests' 'limit=500')
        count=$(jq -r '.rows | length' <<<"$report")
        if [[ "$count" -ge 5 ]]; then
            return
        fi
        sleep 0.25
    done
    fail 'daemon did not persist synthetic requests to metering'
}

assert_advertised_read_actions() {
    local capabilities=$1
    local advertised
    advertised=$(jq -r '
        .action_descriptors
        | to_entries[]
        | select(.value.required_scope == "control:read")
        | .key
    ' <<<"$capabilities" | LC_ALL=C sort)
    local expected
    expected=$(printf '%s\n' \
        agents_list \
        list_models \
        observe_status \
        policy_show \
        policy_status \
        providers_list \
        requests \
        route \
        status | LC_ALL=C sort)
    [[ "$advertised" == "$expected" ]] \
        || fail "an advertised read action lacks an endpoint assertion: $advertised"
}

assert_read_actions() {
    local capabilities
    capabilities=$(control_get "$CLIENT_READ_TOKEN" '/capabilities')
    assert_json "$capabilities" '.protocol == "bitrouter-control" and .protocol_version == 1'
    assert_json "$capabilities" '.scopes == ["control:read"]'
    assert_json "$capabilities" '.resources.capabilities != null and .resources.state != null'
    assert_json "$capabilities" '.action_descriptors.reload == null'
    assert_advertised_read_actions "$capabilities"

    local status
    status=$(control_get "$CLIENT_READ_TOKEN" '/status')
    assert_json "$status" 'has("running")'

    local models
    models=$(control_get "$CLIENT_READ_TOKEN" '/models' 'provider=fixture')
    assert_json "$models" '.models | type == "array"'

    local route
    route=$(control_post "$CLIENT_READ_TOKEN" '/route/preview' \
        '{"model":"fixture-a","prompt":"container acceptance"}')
    assert_json "$route" '.requested_model == "fixture-a" and (.provider_chain | length) > 0'

    local providers
    providers=$(control_get "$CLIENT_READ_TOKEN" '/providers')
    assert_json "$providers" '.resolved_via == "live" and (.providers | type == "array")'

    local observe
    observe=$(control_get "$CLIENT_READ_TOKEN" '/observe/status')
    assert_json "$observe" 'has("daemon_reachable") and has("metrics_enabled")'

    local policy_status
    policy_status=$(control_get "$CLIENT_READ_TOKEN" '/policy/status' 'view=active')
    assert_json "$policy_status" '.view == "active" and .availability == "available"'

    local policy_show
    policy_show=$(control_get "$CLIENT_READ_TOKEN" '/policy/show' 'view=active' 'name=fixture')
    assert_json "$policy_show" '.definitions.fixture.tiers.chosen == "fixture:fixture-a"'

    local agents
    agents=$(control_get "$CLIENT_READ_TOKEN" '/agents')
    assert_json "$agents" '.resolved_via == "live" and (.agents | type == "array")'

    local state
    state=$(control_get "$CLIENT_READ_TOKEN" '/state')
    assert_json "$state" '(.server_instance_id | type == "string") and (.generation | type == "number")'
}

assert_request_filters() {
    local since
    since=$(utc_timestamp -600)
    local until
    until=$(utc_timestamp)
    local report
    report=$(control_get \
        "$CLIENT_READ_TOKEN" \
        '/requests' \
        'limit=1' \
        'model=fixture-a' \
        'provider=fixture' \
        "since=$since" \
        "until=$until")
    local expected_since=${since/Z/+00:00}
    local expected_until=${until/Z/+00:00}
    assert_json "$report" '
        .filters.model == "fixture-a"
        and .filters.provider == "fixture"
        and (.filters.since | type == "string")
        and (.filters.until | type == "string")
        and .rate_scope == "all callers, trailing minute"
        and .metering.summary == "available"
        and .metering.rate == "available"
        and .metering.rows == "available"
        and (.rows | length) == 1
        and .truncated == true
        and .requests >= 3
        and .rows[0].model == "fixture-a"
        and .rows[0].provider == "fixture"
    '
    [[ "$(jq -r '.filters.since' <<<"$report")" == "$expected_since" ]] \
        || fail 'server did not return the resolved inclusive request bound'
    [[ "$(jq -r '.filters.until' <<<"$report")" == "$expected_until" ]] \
        || fail 'server did not return the resolved exclusive request bound'
    printf 'REMOTE_ADMIN_HTTP_REQUEST_FILTER_BOUNDS since=%s until=%s newest_row=%s\n' \
        "$expected_since" \
        "$expected_until" \
        "$(jq -r '.rows[0].created_at' <<<"$report")"
    assert_json "$report" '
        .scope == "all callers"
        and .requests >= 4
        and all(.rows[]; ((.error // "") | contains("fixture-upstream-secret") | not))
    '

    local sanitized
    sanitized=$(control_get \
        "$CLIENT_READ_TOKEN" \
        '/requests' \
        'limit=500' \
        'model=fixture-a' \
        'provider=fixture' \
        "since=$since" \
        "until=$until")
    assert_json "$sanitized" '
        any(.rows[]; .error != null)
        and all(.rows[]; ((.error // "") | contains("fixture-upstream-secret") | not))
    '
}

assert_request_filter_validation() {
    local response
    response=$(control_status "$CLIENT_READ_TOKEN" GET '/requests?limit=0' 400)
    assert_json "$response" '.error.code == "bad_request"'

    response=$(control_status "$CLIENT_READ_TOKEN" GET '/requests?limit=501' 400)
    assert_json "$response" '.error.code == "bad_request"'

    response=$(control_status \
        "$CLIENT_READ_TOKEN" \
        GET \
        '/requests?limit=1&since=2026-01-01T00:00:00Z' \
        400)
    assert_json "$response" '.error.code == "bad_request"'

    response=$(control_status \
        "$CLIENT_READ_TOKEN" \
        GET \
        '/requests?limit=1&since=2026-01-01T00:00:00Z&until=2026-01-09T00:00:01Z' \
        400)
    assert_json "$response" '.error.code == "bad_request"'
}

assert_policy_divergence() {
    local active
    active=$(control_get "$CLIENT_READ_TOKEN" '/policy/status' 'view=active')
    local disk
    disk=$(control_get "$CLIENT_READ_TOKEN" '/policy/status' 'view=disk')
    local active_digest
    active_digest=$(jq -r '.digest' <<<"$active")
    local disk_digest
    disk_digest=$(jq -r '.digest' <<<"$disk")
    [[ "$active_digest" != 'null' && "$disk_digest" != 'null' && "$active_digest" != "$disk_digest" ]] \
        || fail 'staged disk policy did not diverge from the live policy'

    local active_policy
    active_policy=$(control_get "$CLIENT_READ_TOKEN" '/policy/show' 'view=active' 'name=fixture')
    local disk_policy
    disk_policy=$(control_get "$CLIENT_READ_TOKEN" '/policy/show' 'view=disk' 'name=fixture')
    assert_json "$active_policy" '.definitions.fixture.tiers.chosen == "fixture:fixture-a"'
    assert_json "$disk_policy" '.definitions.fixture.tiers.chosen == "fixture:fixture-b"'
}

wait_for_disconnected_admission() {
    local request_id=$1
    local instance=$2
    local report
    for _ in $(seq 1 120); do
        if report=$(control_get \
            "$CLIENT_ADMIN_TOKEN" \
            "/operations/$request_id?instance=$instance" 2>/dev/null); then
            jq --exit-status --arg request_id "$request_id" \
                '.request_id == $request_id' <<<"$report" >/dev/null \
                || fail 'disconnected request returned the wrong operation identity'
            printf '%s' "$report"
            return
        fi
        sleep 0.25
    done
    fail "disconnected reload $request_id was not admitted before recovery"
}

poll_operation() {
    local request_id=$1
    local instance=$2
    local report
    local status
    for _ in $(seq 1 120); do
        report=$(control_get "$CLIENT_ADMIN_TOKEN" "/operations/$request_id?instance=$instance")
        status=$(jq -r '.status' <<<"$report")
        if [[ "$status" != 'running' ]]; then
            printf '%s' "$report"
            return
        fi
        sleep 0.25
    done
    fail "reload operation $request_id did not finish"
}

assert_disconnected_reload_recovery() {
    local state
    state=$(control_get "$CLIENT_ADMIN_TOKEN" '/state')
    local instance
    instance=$(jq -r '.server_instance_id' <<<"$state")
    local generation
    generation=$(jq -r '.generation' <<<"$state")
    local request_id
    request_id=$(python3 -c 'import uuid; print(uuid.uuid4())')
    local body
    body=$(jq --null-input \
        --arg request_id "$request_id" \
        --arg instance "$instance" \
        --argjson generation "$generation" \
        '{request_id: $request_id, expected_server_instance_id: $instance, expected_generation: $generation}')

    local denied
    denied=$(control_status "$CLIENT_READ_TOKEN" POST '/reload' 403 "$body")
    assert_json "$denied" '.error.code == "scope_denied"'

    /usr/local/libexec/disconnect-submit.py "$body"
    local admitted
    admitted=$(wait_for_disconnected_admission "$request_id" "$instance")
    assert_json "$admitted" '.status == "running" or .status == "succeeded"'

    local duplicate
    duplicate=$(control_post "$CLIENT_ADMIN_TOKEN" '/reload' "$body")
    jq --exit-status --arg request_id "$request_id" '.request_id == $request_id' <<<"$duplicate" >/dev/null \
        || fail 'duplicate reload submission did not return the admitted operation'

    local completed
    completed=$(poll_operation "$request_id" "$instance")
    assert_json "$completed" '
        .status == "succeeded"
        and .result != null
        and .completed_at_unix_ms != null
    '

    local denied_operation
    denied_operation=$(control_status \
        "$CLIENT_READ_TOKEN" \
        GET \
        "/operations/$request_id?instance=$instance" \
        403)
    assert_json "$denied_operation" '.error.code == "scope_denied"'

    local after_state
    after_state=$(control_get "$CLIENT_ADMIN_TOKEN" '/state')
    [[ "$(jq -r '.generation' <<<"$after_state")" == "$((generation + 1))" ]] \
        || fail 'duplicate reload submission advanced generation twice'

    local active
    active=$(control_get "$CLIENT_ADMIN_TOKEN" '/policy/status' 'view=active')
    local disk
    disk=$(control_get "$CLIENT_ADMIN_TOKEN" '/policy/status' 'view=disk')
    local active_digest
    active_digest=$(jq -r '.digest' <<<"$active")
    local disk_digest
    disk_digest=$(jq -r '.digest' <<<"$disk")
    [[ "$active_digest" == "$disk_digest" && "$active_digest" != 'null' ]] \
        || fail 'remote reload did not apply the staged policy live'

    local active_policy
    active_policy=$(control_get "$CLIENT_ADMIN_TOKEN" '/policy/show' 'view=active' 'name=fixture')
    assert_json "$active_policy" '.definitions.fixture.tiers.chosen == "fixture:fixture-b"'

    printf 'REMOTE_ADMIN_OPERATION_ID=%s\n' "$request_id"
    printf 'REMOTE_ADMIN_OPERATION_INSTANCE=%s\n' "$instance"
}

assert_successful_cli_reload_and_operation_show() {
    bitrouter context add administrator \
        --endpoint "$control_endpoint" \
        --token-env CLIENT_ADMIN_TOKEN >/tmp/context-administrator.json
    bitrouter --context administrator reload >/tmp/cli-reload.json
    jq --exit-status '
        .status == "succeeded"
        and .result.outcome == "succeeded"
        and (.request_id | type == "string")
        and (.server_instance_id | type == "string")
        and .completed_at_unix_ms != null
    ' /tmp/cli-reload.json >/dev/null \
        || fail 'remote CLI reload did not return a successful retained operation'

    local request_id
    request_id=$(jq -r '.request_id' /tmp/cli-reload.json)
    local instance
    instance=$(jq -r '.server_instance_id' /tmp/cli-reload.json)
    bitrouter --context administrator operations show "$request_id" \
        --instance "$instance" >/tmp/cli-operations-show.json
    jq --exit-status --arg request_id "$request_id" --arg instance "$instance" '
        .request_id == $request_id
        and .server_instance_id == $instance
        and .status == "succeeded"
        and .result.outcome == "succeeded"
        and .completed_at_unix_ms != null
    ' /tmp/cli-operations-show.json >/dev/null \
        || fail 'remote CLI operations show did not recover the successful operation'
}

assert_boot_change() {
    local prior_request_id=$1
    local prior_instance=$2
    local state
    state=$(control_get "$CLIENT_ADMIN_TOKEN" '/state')
    local current_instance
    current_instance=$(jq -r '.server_instance_id' <<<"$state")
    [[ "$current_instance" != "$prior_instance" ]] \
        || fail 'server restart did not issue a new server instance id'

    local prior
    prior=$(control_status \
        "$CLIENT_ADMIN_TOKEN" \
        GET \
        "/operations/$prior_request_id?instance=$prior_instance" \
        409)
    assert_json "$prior" '.error.code == "server_instance_changed"'
}

assert_legacy_token_read_only() {
    bitrouter context add legacy \
        --endpoint "$control_endpoint" \
        --token-env CLIENT_READ_TOKEN >/tmp/context-legacy.json
    bitrouter --context legacy status >/tmp/legacy-status.json
    jq --exit-status 'has("running")' /tmp/legacy-status.json >/dev/null \
        || fail 'legacy control token could not read status'

    local capabilities
    capabilities=$(control_get "$CLIENT_READ_TOKEN" '/capabilities')
    assert_json "$capabilities" '
        .scopes == ["control:read"]
        and .action_descriptors.reload == null
    '

    local state
    state=$(control_get "$CLIENT_READ_TOKEN" '/state')
    local body
    body=$(jq --null-input \
        --arg request_id "$(python3 -c 'import uuid; print(uuid.uuid4())')" \
        --arg instance "$(jq -r '.server_instance_id' <<<"$state")" \
        --argjson generation "$(jq -r '.generation' <<<"$state")" \
        '{request_id: $request_id, expected_server_instance_id: $instance, expected_generation: $generation}')

    local denied
    denied=$(control_status "$CLIENT_READ_TOKEN" POST '/reload' 403 "$body")
    assert_json "$denied" '.error.code == "scope_denied"'

    local old_admin
    old_admin=$(control_status "$CLIENT_ADMIN_TOKEN" GET '/status' 401)
    assert_json "$old_admin" '.error.code == "unauthorized"'

    if bitrouter --context legacy reload >/tmp/legacy-reload.json 2>/tmp/legacy-reload.err; then
        fail 'legacy read-only context unexpectedly reloaded the router'
    fi
}

assert_partial_reload_cli() {
    bitrouter context add administrator \
        --endpoint "$control_endpoint" \
        --token-env CLIENT_ADMIN_TOKEN >/tmp/context-administrator.json
    if bitrouter --context administrator reload >/tmp/partial-reload.json 2>/tmp/partial-reload.err; then
        fail 'CLI reported success for a partially-applied reload'
    fi
    if ! jq --slurp --exit-status '
        length == 2
        and .[0].status == "partially_applied"
        and .[0].result.outcome == "partially_applied"
        and any(.[0].result.participants[]; .outcome == "applied")
        and any(
            .[0].result.participants[];
            .outcome == "failed"
            and .error.code == "fixture_policy_failure"
        )
        and .[1].error.kind == "internal"
        and (.[1].error.message | contains("partially applied"))
    ' /tmp/partial-reload.json >/dev/null; then
        printf '%s\n' 'partial reload stdout:' >&2
        cat /tmp/partial-reload.json >&2
        printf '%s\n' 'partial reload stderr:' >&2
        cat /tmp/partial-reload.err >&2
        fail 'CLI did not render the ordered partial reload report and error envelope'
    fi
}

main() {
    local phase=${1-read}
    assert_client_boundary
    case "$phase" in
        read)
            assert_cli_reaches_tls_proxy
            assert_control_is_loopback_only
            seed_requests
            wait_for_metering
            assert_read_actions
            assert_cli_read_actions
            # This remains mandatory in the normal acceptance run. The opt-out
            # only lets an MCP transport failure be diagnosed without hiding
            # the independent CLI and dashboard journeys.
            if [[ "${BITROUTER_REMOTE_ADMIN_SKIP_MCP:-0}" != "1" ]]; then
                assert_mcp_control_tools
            fi
            assert_remote_target_isolation
            assert_request_filters
            assert_request_filter_validation
            ;;
        diverged)
            assert_policy_divergence
            ;;
        reload)
            [[ -n "${CLIENT_ADMIN_TOKEN-}" ]] || fail 'admin token is required for reload acceptance'
            assert_disconnected_reload_recovery
            ;;
        cli-reload)
            [[ -n "${CLIENT_ADMIN_TOKEN-}" ]] || fail 'admin token is required for CLI reload acceptance'
            assert_successful_cli_reload_and_operation_show
            ;;
        boot-change)
            [[ -n "${CLIENT_ADMIN_TOKEN-}" ]] || fail 'admin token is required for boot-change acceptance'
            [[ $# -eq 3 ]] || fail 'boot-change requires request id and prior instance'
            assert_boot_change "$2" "$3"
            ;;
        legacy)
            [[ -n "${CLIENT_ADMIN_TOKEN-}" ]] || fail 'admin token is required for legacy-token acceptance'
            assert_legacy_token_read_only
            ;;
        partial)
            [[ -n "${CLIENT_ADMIN_TOKEN-}" ]] || fail 'admin token is required for partial reload acceptance'
            assert_partial_reload_cli
            ;;
        *)
            fail "unknown acceptance phase $phase"
            ;;
    esac
}

main "$@"
