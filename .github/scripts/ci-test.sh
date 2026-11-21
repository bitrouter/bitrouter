#!/usr/bin/env bash
# Run a suite from the verified download, reusing one extracted tree at a time.
set -euo pipefail

run_database() {
  local backend="${1:?database backend required}" url test
  case "$backend" in
    postgres) url='postgres://fixture:fixture-password@127.0.0.1:5432/bitrouter_test' ;;
    mysql) url='mysql://fixture:fixture-password@127.0.0.1:3306/bitrouter_test' ;;
    *) echo "Unknown database backend: $backend" >&2; return 1 ;;
  esac
  BITROUTER_TEST_MIGRATION_DATABASE_URL="$url" db_test "kind(lib) & test(=db::migration::baseline::server_tests::${backend}_current_baseline_catalog_and_refusal)"
  export BITROUTER_TEST_DATABASE_URL="$url"

  # Verify tenant eval conflicts preserve the transaction and creator.
  db_test "(kind(lib)) & (test(=eval::store::scope_tests::server_immutable_conflicts_preserve_transaction_and_first_creator))"

  # Verify tenant eval composites share one stable transaction.
  db_test "(kind(lib)) & (test(=eval::store::owned_tests::server::lifecycle::server_owned_eval_committed_replay_preserves_provenance_and_snapshot) | test(=eval::store::owned_tests::server::lifecycle::server_owned_eval_external_rr_winner_is_rejected_without_replay_or_events) | test(=eval::store::owned_tests::server::lifecycle::server_owned_eval_freeze_keeps_rr_membership_across_external_admission_commit) | test(=eval::store::owned_tests::server::lifecycle::server_owned_eval_terminal_fault_rolls_back_result_and_prior_dispute) | test(=eval::store::owned_tests::server::server_owned_eval_composites_share_rr_transaction_and_scoped_winner_reads))"

  # Verify directory projections and receipt history on the selected backend.
  db_test "(kind(lib)) & (test(=tenancy::tests::execution::evaluation::server::server_owned_trajectory_directory_projection_preserves_integer_decoders_and_bounds) | test(=tenancy::tests::execution::evaluation::server::server_owned_trajectory_empty_source_compatibility_refuses_retained_rows) | test(=tenancy::tests::execution::evaluation::server::server_owned_trajectory_known_receipts_drain_restart_and_scan_with_one_reservation))"

  # Verify owned HTTP continuation consumers on the selected backend.
  db_test "(kind(lib)) & (test(=tenancy::tests::execution::http_continuation::server::server_http034_consumer_exact_cas_absence_race_and_restart))"

  # Verify current execution receipt persistence on the selected backend.
  db_test "(kind(lib)) & (test(=tenancy::receipts::store::tests::server::server_receipts_commit_rollback_reconnect_scope_and_immutable_conflict))"

  # Verify current owned OAuth and compound receipts on the selected backend.
  db_test "(kind(lib)) & (test(=tenancy::oauth::storage::tests::server::owned_oauth_slot_and_auxiliary_receipt_real_commit_rollback_reconnect))"

  # Verify MySQL transaction snapshot boundaries.
  if [[ "$backend" == mysql ]]; then
    db_test "(kind(lib)) & (test(=gateway::observability::inventory::server_tests::mysql_tests::locked_gateway_inventory_sees_new_and_changed_rows_after_older_snapshots))"
  fi

  # Verify admission and stream pressure across official gateway processes.
  if [[ "$backend" == postgres ]]; then
    db_test "(kind(lib)) & (test(=gateway::shared::store::tests::postgres_due_states_retries_serialization_abort_with_filtered_fresh_scan) | test(=gateway::shared::store::tests::postgres_get_retries_serialization_abort_with_a_fresh_transaction) | test(=gateway::shared::store::tests::postgres_retry_uses_deadline_and_never_retries_uncertain_errors))"
    db_test "(kind(lib)) & (test(=gateway::shared::runtime::process_tests::bootstrap::admission::official_http2_tenant_and_account_concurrency_cross_replicas))"
    db_test "(kind(lib)) & (test(=gateway::shared::runtime::process_tests::bootstrap::admission::concurrent_static::official_concurrent_static_http1_single_instance) | test(=gateway::shared::runtime::process_tests::bootstrap::admission::concurrent_static::official_concurrent_static_http1_two_instances) | test(=gateway::shared::runtime::process_tests::bootstrap::admission::concurrent_static::official_concurrent_static_http2_single_instance) | test(=gateway::shared::runtime::process_tests::bootstrap::admission::concurrent_static::official_concurrent_static_http2_two_instances))"
    # Keep the real-socket stream regression in PR coverage even though
    # the complete packaged-process campaign runs separately.
    db_test "(kind(lib)) & (test(=gateway::shared::runtime::process_tests::bootstrap::admission::recovery::provider::slow_reader::official_stalled_response_reader_releases_local_capacity_without_provider_success))"
  fi

  # Verify PostgreSQL credential and transaction regressions.
  if [[ "$backend" == postgres ]]; then
    db_test "(kind(lib)) & (test(=gateway::shared::schema::current_validation::tests::ut_postgres_shared_schema_accepts_current_fixture) | test(=gateway::shared::schema::current_validation::tests::ut_postgres_shared_schema_composition_refuses_foreign_objects) | test(=gateway::shared::schema::current_validation::tests::ut_postgres_shared_schema_composition_refuses_mutated_shared_layout) | test(=gateway::shared::schema::current_validation::tests::ut_postgres_shared_schema_composition_restarts_ordinary_baseline) | test(=gateway::shared::schema::current_validation::tests::ut_postgres_shared_schema_refuses_corrupt_uniqueness) | test(=gateway::shared::schema::current_validation::tests::ut_postgres_shared_schema_refuses_filters_constraints_and_indexes) | test(=gateway::shared::schema::current_validation::tests::ut_postgres_shared_schema_refuses_native_continuation_columns))"
    db_test "(kind(lib)) & (test(=gateway::shared::tests::evaluation::preflight::postgres_shared_eval_subject_prejoin))"
    db_test "(kind(lib)) & (test(=gateway::shared::authentication::concurrency_tests::postgres_static_reads_cross_independent_pools) | test(=gateway::shared::authentication::concurrency_tests::postgres_static_reads_share_one_pool))"
    db_test "(kind(lib)) & (test(=gateway::shared::engine::admission::contention_tests::postgres_admission_survives_heartbeat_single_owner) | test(=gateway::shared::engine::admission::contention_tests::postgres_admission_survives_heartbeat_two_owners))"
    db_test "(kind(lib)) & (test(=gateway::shared::engine::attempts::contention_tests::postgres_dispatch_shares_resource_fence_single_owner) | test(=gateway::shared::engine::attempts::contention_tests::postgres_dispatch_shares_resource_fence_two_owners))"
    db_test "(kind(lib)) & (test(=gateway::shared::engine::contract_tests::postgres_capture_identity_rejects_before_attempt_or_terminal_writes) | test(=gateway::shared::engine::contract_tests::postgres_invalid_source_publication_rejection_preserves_authority) | test(=gateway::shared::engine::contract_tests::postgres_join_checks_old_retained_publication_and_terminal_beyond_first_page) | test(=gateway::shared::engine::contract_tests::postgres_read_committed_terminal_invalidates_serializable_execution_snapshot))"
  fi

  # Exercise current schema and gateway persistence on the selected backend.
  db_test "(kind(lib)) & (test(=tests::database_backends::server_database_behaviors))"

  # Verify competing inventory controllers across reconnect.
  case "$backend" in
    postgres) test=postgres_competing_gateway_controllers_share_one_durable_reservation ;;
    mysql) test=mysql_tests::competing_gateway_controllers_share_one_durable_reservation ;;
    *) exit 1 ;;
  esac
  db_test "(kind(lib)) & (test(=gateway::observability::inventory::server_tests::$test))"
}

db_test() {
  bash "$0" service --test-threads 1 --run-ignored only -E "$1"
}

if [[ "${1:-}" == database ]]; then
  run_database "${2:-}"
  exit
fi

suite="${1:?suite required}"
shift
workspace="${GITHUB_WORKSPACE:?workspace required}"
restore=ci-tests
target="$restore/target"
profile=ci
suffix=
if [[ "$RUNNER_OS" == Windows ]]; then suffix=.exe; fi

case "$suite" in
  service-minimal|service-default|service-shared) profile="ci-$suite" ;;
  service|service-grpc|bro) ;;
  *) echo "Unknown CI suite: $suite" >&2; exit 1 ;;
esac
if [[ ! -f "$restore/suite" || "$(cat "$restore/suite")" != "$suite" ]]; then
  rm -rf "$restore"
  mkdir -p "$restore"
  cargo-nextest nextest list --list-type binaries-only \
    --archive-file "ci-artifacts/$suite.tar.zst" --extract-to "$workspace/$restore" \
    --workspace-remap "$workspace" >/dev/null
  printf '%s\n' "$suite" > "$restore/suite"
fi

export BITROUTER_TEST_PREBUILT_ONLY=1
export BITROUTER_TEST_EXAMPLES_DIR="$workspace/$target/debug/examples"
case "$suite" in
  bro)
    export BITROUTER_TEST_BRO="$workspace/$target/debug/bro$suffix"
    test -f "$BITROUTER_TEST_EXAMPLES_DIR/bro_acp_fixture$suffix"
    "$BITROUTER_TEST_BRO" build-info | jq -e --arg sha "$GITHUB_SHA" '
      .product == "bitrouter-bro" and .source_revision == $sha'
    ;;
  service-grpc) ;; # This suite builds and runs library tests only.
  *)
    export BITROUTER_TEST_GATEWAY_BINARY="$workspace/$target/debug/bitrouter$suffix"
    "$BITROUTER_TEST_GATEWAY_BINARY" build-info | jq -e --arg sha "$GITHUB_SHA" '
      .product == "bitrouter" and .source_revision == $sha'
    ;;
esac

exec cargo-nextest nextest run \
  --cargo-metadata "$workspace/$target/nextest/cargo-metadata.json" \
  --binaries-metadata "$workspace/$target/nextest/binaries-metadata.json" \
  --workspace-remap "$workspace" --target-dir-remap "$workspace/$target" \
  --profile "$profile" --retries 0 --no-fail-fast --no-tests fail "$@"
