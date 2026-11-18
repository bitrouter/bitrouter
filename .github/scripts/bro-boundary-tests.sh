#!/usr/bin/env bash
set -euo pipefail
suite=${1:?BRO boundary suite required}
exact() {
  local binary=$1
  shift
  bash .github/scripts/ci-test.sh "$suite" --exact-tests "binary(=$binary)" ordinary "$@"
}
case "$suite" in
  core-tree)
    exact managed_native full_tree_runs_four_models_and_delivers_each_conclusion_once \
      the_tree_shares_one_model_attempt_budget_across_all_workers
    exact bro runner_tests::confirmed_child_and_terminal_recover_on_either_side_of_a_lost_ack
    ;;
  checkpoints)
    exact managed_native large_cold_checkpoint_keeps_multiturn_context_and_sixteen_step_tool_loop
    exact native_boundaries directory_sync_failure_blocks_replay_before_and_after_file_publication
    exact bro tools::tests::read_byte_pages_are_bounded_and_large_records_fail
    ;;
  bro-process)
    exact managed_native active_child_cancellation_fences_paid_dispatch_and_overlap_on_restart \
      approved_write_reads_and_continues_then_cancels_the_next_approval
    exact native_boundaries unknown_paid_execution_fences_overlapping_grants_across_processes \
      write_and_edit_publish_nested_file_before_checkpoint_confirmation
    exact orchestration_recovery child_delegation_wait_wake_and_cancel_survive_restarts \
      transferred_prefix_and_call_pair_are_checked_before_binding
    exact native_acp native_stdio_streams_and_reopens_persisted_history_without_resending \
      cold_load_reissues_the_exact_permission_and_cancel_prevents_an_effect \
      cancelling_a_paid_stream_keeps_its_receipt_and_blocks_cold_redispatch \
      manager_eof_during_stream_preserves_the_complete_message_for_cold_replay
    exact acp_serve routed_serve_gates_provider_methods_and_forwards_manager_permissions \
      manager_eof_reaps_adapter_descendant_during_unanswered_prompt \
      adapter_disconnect_exits_with_manager_stdin_still_open \
      manager_cancel_reaches_pending_prompt_and_leaves_session_usable
    exact bro runner_tests::truncated_paid_stream_refuses_any_replay \
      runner_tests::lost_worker_with_tool_intent_blocks_even_after_cancel \
      runner_tests::approval_restart_keeps_confirmed_results_and_continuity \
      runner_tests::malformed_success_preserves_each_unambiguous_receipt_independently \
      runner_tests::permission_denial_after_dispatch_preserves_receipts_and_ownership \
      runner_tests::cancelling_waiting_approval_settles_without_effects
    ;;
  *) exit 2 ;;
esac
