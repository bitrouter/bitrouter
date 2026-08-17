from __future__ import annotations

import importlib.util
import json
import math
import subprocess
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "terminal_bench_route_evidence.py"
SPEC = importlib.util.spec_from_file_location("terminal_bench_route_evidence", SCRIPT)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError("could not load terminal benchmark evidence adapter")
adapter = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(adapter)


DIGEST_A = "sha256:" + "a" * 64
REPO_ROOT = SCRIPT.parents[3]


def decision(
    decision_id: str,
    *,
    route: str = "agent_route/v1|unknown|mechanical|normal",
    tier: str = "economy",
    captured_at: str = "2026-08-17T00:00:00Z",
) -> dict[str, object]:
    return {
        "decision_id": decision_id,
        "policy": "auto",
        "policy_digest": DIGEST_A,
        "route_projection": route,
        "request_key": route,
        "selected_tier": tier,
        "baseline_tier": "strong",
        "static_tier": tier,
        "captured_at": captured_at,
        "exact_task_id": "task-pass",
        "exact_trial_id": "task-pass__trial:result-task-pass",
        "physical_request_id": f"physical-{decision_id}",
        "task_join_method": "full_messages_prefix",
        "exact_ingress_join": True,
        "cost_micro_usd": 20,
        "latency_ms": 40,
        "progress_clause_ids": [],
    }


def task_result(
    task_id: str = "task-pass",
    reward: int | None = 1,
    exception: dict[str, str] | None = None,
) -> dict[str, object]:
    verifier = None if reward is None else {"rewards": {"reward": reward}}
    return {
        "id": f"result-{task_id}",
        "task_name": f"terminal-bench/{task_id}",
        "task_checksum": task_id,
        "trial_name": f"{task_id}__trial",
        "verifier_result": verifier,
        "exception_info": exception,
        "finished_at": "2026-08-17T00:01:00Z",
    }


def write_jsonl(path: Path, rows: list[dict[str, object]]) -> None:
    path.write_text(
        "".join(json.dumps(row, sort_keys=True) + "\n" for row in rows),
        encoding="utf-8",
    )


def install_trial(
    run_dir: Path,
    task_id: str,
    messages: list[dict[str, object]],
    *,
    trial_name: str | None = None,
    result_id: str | None = None,
    reward: float | int | None = 1,
    critical_violations: int | None = 0,
) -> None:
    trial_name = trial_name or f"{task_id}__trial"
    trial_dir = run_dir / "jobs" / "job" / trial_name
    (trial_dir / "agent").mkdir(parents=True)
    raw = task_result(task_id, reward)
    raw["trial_name"] = trial_name
    raw["id"] = result_id or f"result-{trial_name}"
    if critical_violations is not None:
        raw["critical_violations"] = critical_violations
    (trial_dir / "result.json").write_text(json.dumps(raw), encoding="utf-8")
    steps = [
        {
            "source": "user" if message["role"] == "user" else "agent",
            "message": message["content"],
        }
        for message in messages
    ]
    (trial_dir / "agent" / "trajectory.json").write_text(
        json.dumps({"steps": steps}), encoding="utf-8"
    )


def task_messages(task_id: str, request_count: int) -> list[dict[str, object]]:
    history: list[dict[str, object]] = []
    for index in range(request_count):
        content = (
            f"Task Description:\nBuild {task_id}\n\nCurrent terminal state:\n$"
            if index == 0
            else f"continue {task_id} {index}"
        )
        history.append({"role": "user", "content": content})
        if index + 1 < request_count:
            history.append({"role": "assistant", "content": f"step {index}"})
    return history


def raw_inputs(
    task_id: str,
    messages: list[dict[str, object]],
    *,
    tiers: list[str] | None = None,
    routes: list[str] | None = None,
    errors: list[str | None] | None = None,
    real_decision_schema: bool = False,
) -> tuple[list[dict[str, object]], list[dict[str, object]], list[dict[str, object]]]:
    prefixes = [messages[: index + 1] for index, row in enumerate(messages) if row["role"] == "user"]
    tiers = tiers or ["economy"] * len(prefixes)
    routes = routes or ["agent_route/v1|unknown|mechanical|normal"] * len(prefixes)
    errors = errors or [None] * len(prefixes)
    traces: list[dict[str, object]] = []
    decisions: list[dict[str, object]] = []
    outcomes: list[dict[str, object]] = []
    for index, prefix in enumerate(prefixes):
        request_id = f"physical-{task_id}-{index}"
        traces.append({"id": request_id, "raw_body": {"messages": prefix}})
        row = decision(
            f"decision-{task_id}-{index}",
            route=routes[index],
            tier=tiers[index],
            captured_at=f"2026-08-17T00:00:{index:02d}Z",
        )
        row.pop("exact_task_id")
        row.pop("exact_trial_id")
        row.pop("physical_request_id")
        row.pop("task_join_method")
        row.pop("exact_ingress_join")
        row["ingress_request_id_sha256"] = adapter.ingress_commitment(request_id)
        if real_decision_schema:
            row["request_id"] = row.pop("decision_id")
        decisions.append(row)
        outcomes.append(
            {
                "request_id": request_id,
                "error": errors[index],
                "nominal_cost_usd": "0.000020",
                "decision_at": f"2026-08-17T00:00:{index:02d}Z",
                "created_at": f"2026-08-17T00:00:{index + 1:02d}Z",
                "prompt_tokens": 10,
                "completion_tokens": 2,
            }
        )
    return traces, decisions, outcomes


def run_fixture(
    root: Path,
    run_dir: Path,
    traces: list[dict[str, object]],
    decisions: list[dict[str, object]],
    outcomes: list[dict[str, object]],
) -> Path:
    traces_path = root / "traces.jsonl"
    decisions_path = root / "decisions.jsonl"
    outcomes_path = root / "request-outcomes.jsonl"
    output_dir = root / "output"
    write_jsonl(traces_path, traces)
    write_jsonl(decisions_path, decisions)
    write_jsonl(outcomes_path, outcomes)
    adapter.run(run_dir, decisions_path, output_dir, traces_path, outcomes_path)
    return output_dir


class ErrorTaxonomyTests(unittest.TestCase):
    def test_every_non_task_class_has_a_specific_rule(self) -> None:
        cases = [
            ({"error": "upstream_unavailable"}, ("provider", "provider.upstream-unavailable.v1")),
            ({"error": "dns lookup failed"}, ("network", "network.dns.v1")),
            ({"status": 401, "error": "upstream unavailable"}, ("auth", "auth.http-401-403.v1")),
            ({"status": 429, "error": "upstream unavailable"}, ("rate_limit", "rate-limit.http-429.v1")),
            ({"error": "connection reset by peer"}, ("transport", "transport.connection-reset.v1")),
        ]

        for raw, expected in cases:
            with self.subTest(raw=raw):
                observed = adapter.classify_request_error(raw)
                self.assertEqual((observed.category, observed.rule_id), expected)

    def test_specific_status_rule_precedes_broad_provider_text(self) -> None:
        auth = adapter.classify_request_error(
            {"status": 403, "error": "provider upstream unavailable"}
        )
        rate_limit = adapter.classify_request_error(
            {"status": 429, "error": "provider upstream unavailable"}
        )

        self.assertEqual(auth.category, "auth")
        self.assertEqual(rate_limit.category, "rate_limit")

    def test_agent_timeout_is_not_a_request_transport_error(self) -> None:
        observed = adapter.classify_request_error(
            {"exception_type": "AgentTimeoutError", "exception_message": "agent timed out"}
        )

        self.assertIsNone(observed.category)


class PacketAttributionTests(unittest.TestCase):
    def test_single_cell_task_credits_only_the_earliest_decision(self) -> None:
        decisions = [
            decision("decision-b", captured_at="2026-08-17T00:00:02Z"),
            decision("decision-a", captured_at="2026-08-17T00:00:01Z"),
        ]

        packet, evidence = adapter.build_packet(task_result(), decisions, [])

        self.assertEqual(packet["subject"]["scope"], "task")
        self.assertEqual(packet["result"]["verdict"], "pass")
        self.assertEqual(packet["result"]["evaluator"]["kind"], "task_native")
        self.assertEqual(
            packet["result"]["decision_credit"],
            {
                "decision-a": {
                    "weight_ppm": 1_000_000,
                    "metric_ids": ["quality.pass"],
                }
            },
        )
        self.assertTrue(evidence["quality_credit_eligible"])
        self.assertEqual(evidence["representative_decision_id"], "decision-a")

    def test_multi_cell_task_is_preserved_but_quality_is_inconclusive(self) -> None:
        decisions = [
            decision("decision-a"),
            decision(
                "decision-b",
                route="agent_route/v1|unknown|verify|normal",
                tier="balanced",
                captured_at="2026-08-17T00:00:02Z",
            ),
        ]

        packet, evidence = adapter.build_packet(task_result(), decisions, [])

        self.assertEqual(packet["result"]["verdict"], "inconclusive")
        self.assertEqual(packet["result"]["decision_credit"], {})
        self.assertFalse(evidence["quality_credit_eligible"])
        self.assertEqual(evidence["attribution_reason"], "multiple_route_cells")

    def test_missing_exact_ingress_join_withholds_quality(self) -> None:
        inexact = decision("decision-a")
        inexact["exact_ingress_join"] = False

        packet, evidence = adapter.build_packet(task_result(), [inexact], [])

        self.assertEqual(packet["result"]["verdict"], "inconclusive")
        self.assertEqual(packet["result"]["decision_credit"], {})
        self.assertFalse(evidence["quality_credit_eligible"])
        self.assertEqual(evidence["attribution_reason"], "exact_decision_join_missing")

    def test_missing_exact_task_id_withholds_quality(self) -> None:
        inexact = decision("decision-a")
        del inexact["exact_task_id"]

        packet, evidence = adapter.build_packet(task_result(), [inexact], [])

        self.assertEqual(packet["result"]["verdict"], "inconclusive")
        self.assertEqual(packet["result"]["decision_credit"], {})
        self.assertEqual(evidence["attribution_reason"], "exact_decision_join_missing")

    def test_non_finite_reward_is_rejected(self) -> None:
        raw = task_result(reward=1)
        raw["verifier_result"] = {"rewards": {"reward": math.nan}}

        with self.assertRaisesRegex(ValueError, "finite"):
            adapter.classify_outcome(raw)

    def test_recovered_provider_failure_keeps_terminal_pass_without_quality_credit(self) -> None:
        request_error = adapter.classify_request_error(
            {"error": "upstream_unavailable"}
        )

        packet, evidence = adapter.build_packet(
            task_result(), [decision("decision-a")], [request_error]
        )

        self.assertEqual(evidence["terminal_verdict"], "pass")
        self.assertEqual(packet["result"]["verdict"], "pass")
        self.assertEqual(
            packet["result"]["decision_credit"],
            {
                "decision-a": {
                    "weight_ppm": 0,
                    "metric_ids": ["quality.pass"],
                }
            },
        )
        self.assertFalse(evidence["quality_credit_eligible"])
        self.assertEqual(evidence["excluded_non_task_errors"], 1)

    def test_provider_exception_without_verifier_is_inconclusive(self) -> None:
        raw = task_result(
            reward=None,
            exception={
                "exception_type": "APIError",
                "exception_message": "upstream unavailable",
            },
        )

        outcome = adapter.classify_outcome(raw)

        self.assertEqual(outcome.terminal_verdict, "inconclusive")
        self.assertEqual(outcome.excluded_error.category, "provider")


class ExactJoinTests(unittest.TestCase):
    def test_full_prefix_and_unique_task_description_join_without_time(self) -> None:
        task_a = [
            {
                "role": "user",
                "content": "Task Description:\nBuild A\n\nCurrent terminal state:\n$",
            },
            {"role": "assistant", "content": "working"},
            {"role": "user", "content": "next"},
        ]
        task_b = [
            {
                "role": "user",
                "content": "Task Description:\nBuild B\n\nCurrent terminal state:\n$",
            }
        ]
        traces = [
            {"id": "request-a", "raw_body": {"messages": task_a}},
            {
                "id": "request-b",
                "raw_body": {
                    "messages": [
                        {
                            "role": "user",
                            "content": "Task Description:\nBuild B\n\nCurrent terminal state:\nchanged",
                        }
                    ]
                },
            },
        ]
        decisions = [
            {
                **decision("decision-a"),
                "ingress_request_id_sha256": adapter.ingress_commitment("request-a"),
            },
            {
                **decision("decision-b"),
                "ingress_request_id_sha256": adapter.ingress_commitment("request-b"),
            },
        ]

        joined, summary = adapter.join_raw_decisions(
            {"task-a": task_a, "task-b": task_b}, traces, decisions
        )

        self.assertEqual(
            [(row["decision_id"], row["exact_task_id"], row["task_join_method"]) for row in joined],
            [
                ("decision-a", "task-a", "full_messages_prefix"),
                ("decision-b", "task-b", "task_description_field"),
            ],
        )
        self.assertEqual(summary["joined"], 2)
        self.assertEqual(summary["ambiguous"], 0)

    def test_duplicate_task_description_is_ambiguous_and_withheld(self) -> None:
        common = [
            {
                "role": "user",
                "content": "Task Description:\nSame task\n\nCurrent terminal state:\n$",
            }
        ]
        changed = [
            {
                "role": "user",
                "content": "Task Description:\nSame task\n\nCurrent terminal state:\nchanged",
            }
        ]
        traces = [{"id": "request-a", "raw_body": {"messages": changed}}]
        decisions = [
            {
                **decision("decision-a"),
                "ingress_request_id_sha256": adapter.ingress_commitment("request-a"),
            }
        ]

        joined, summary = adapter.join_raw_decisions(
            {"task-a": common, "task-b": common}, traces, decisions
        )

        self.assertEqual(joined, [])
        self.assertEqual(summary["ambiguous"], 1)


class MatrixTests(unittest.TestCase):
    def test_strict_and_observational_gates_never_mix(self) -> None:
        economy_route = "agent_route/v1|unknown|mechanical|normal"
        balanced_route = "agent_route/v1|unknown|implement|normal"
        evidence: list[dict[str, object]] = []
        for index in range(5):
            evidence.append(
                {
                    "task_id": f"strict-{index}",
                    "route_projection": economy_route,
                    "policy": "auto",
                    "terminal_verdict": "pass",
                    "quality_credit_eligible": True,
                    "selected_tiers": {"economy": 1},
                    "static_tiers": {"economy": 1},
                    "cost_micro_usd": 20,
                    "latency_ms": 40,
                    "guard_promotions": 0,
                    "excluded_non_task_errors": 0,
                    "excluded_error_rule_ids": [],
                    "attribution_ambiguity": False,
                    "recovery_dependency": False,
                    "critical_violations": 0,
                    "critical_violations_known": True,
                    "associated_terminal_pass": True,
                    "associated_terminal_fail": False,
                }
            )
            evidence.append(
                {
                    "task_id": f"screen-{index}",
                    "route_projection": balanced_route,
                    "policy": "auto",
                    "terminal_verdict": "inconclusive",
                    "quality_credit_eligible": False,
                    "selected_tiers": {"balanced": 1},
                    "static_tiers": {"balanced": 1},
                    "cost_micro_usd": 30,
                    "latency_ms": 50,
                    "guard_promotions": 0,
                    "excluded_non_task_errors": 0,
                    "excluded_error_rule_ids": [],
                    "attribution_ambiguity": True,
                    "recovery_dependency": False,
                    "critical_violations": 0,
                    "critical_violations_known": True,
                    "associated_terminal_pass": True,
                    "associated_terminal_fail": False,
                }
            )

        matrix = {
            row["route_projection"]: row
            for row in adapter.build_experience_matrix(evidence)
        }
        economy = matrix[economy_route]
        balanced = matrix[balanced_route]

        self.assertEqual(economy["independent_tasks"], 5)
        self.assertEqual(economy["pass_rate_ppm"], 1_000_000)
        self.assertEqual(economy["active_recommendation"], "economy")
        self.assertTrue(economy["quality_credit_eligible"])
        self.assertFalse(economy["controlled_validation_candidate"])
        self.assertEqual(balanced["active_recommendation"], "retain")
        self.assertFalse(balanced["quality_credit_eligible"])
        self.assertTrue(balanced["economy_experiment_candidate"])
        self.assertTrue(balanced["controlled_validation_candidate"])
        self.assertEqual(
            balanced["screening_reason"], "balanced_normal_observational"
        )

    def test_route_errors_block_direct_and_screening_candidates(self) -> None:
        route = "agent_route/v1|unknown|mechanical|normal"
        evidence = []
        for index in range(5):
            row = {
                "task_id": f"task-{index}",
                "route_projection": route,
                "policy": "auto",
                "terminal_verdict": "pass",
                "quality_credit_eligible": True,
                "selected_tiers": {"economy": 1},
                "static_tiers": {"economy": 1},
                "cost_micro_usd": 20,
                "latency_ms": 40,
                "guard_promotions": 0,
                "excluded_non_task_errors": int(index == 0),
                "excluded_error_rule_ids": (
                    ["provider.upstream-unavailable.v1"] if index == 0 else []
                ),
                "attribution_ambiguity": False,
                "recovery_dependency": False,
                "critical_violations": 0,
                "critical_violations_known": True,
                "associated_terminal_pass": True,
                "associated_terminal_fail": False,
            }
            evidence.append(row)

        result = adapter.build_experience_matrix(evidence)[0]

        self.assertEqual(result["active_recommendation"], "retain")
        self.assertFalse(result["controlled_validation_candidate"])
        self.assertEqual(result["screening_reason"], "route_non_task_errors")

    def test_observational_terminal_failure_blocks_controlled_validation(self) -> None:
        route = "agent_route/v1|unknown|implement|normal"
        evidence = []
        for index in range(5):
            evidence.append(
                {
                    "task_id": f"passing-{index}",
                    "route_projection": route,
                    "policy": "auto",
                    "terminal_verdict": "inconclusive",
                    "quality_credit_eligible": False,
                    "selected_tiers": {"balanced": 1},
                    "static_tiers": {"balanced": 1},
                    "cost_micro_usd": 10,
                    "latency_ms": 20,
                    "guard_promotions": 0,
                    "excluded_non_task_errors": 0,
                    "excluded_error_rule_ids": [],
                    "attribution_ambiguity": True,
                    "recovery_dependency": False,
                    "critical_violations": 0,
                    "critical_violations_known": True,
                    "associated_terminal_pass": True,
                    "associated_terminal_fail": False,
                }
            )
        evidence.append(
            {
                **evidence[0],
                "task_id": "failing-task",
                "associated_terminal_pass": False,
                "associated_terminal_fail": True,
            }
        )

        result = adapter.build_experience_matrix(evidence)[0]

        self.assertFalse(result["controlled_validation_candidate"])
        self.assertEqual(result["screening_reason"], "terminal_failures")

    def test_unknown_critical_evidence_blocks_both_recommendation_layers(self) -> None:
        route = "agent_route/v1|unknown|mechanical|normal"
        evidence = []
        for index in range(5):
            evidence.append(
                {
                    "task_id": f"task-{index}",
                    "route_projection": route,
                    "policy": "auto",
                    "terminal_verdict": "pass",
                    "quality_credit_eligible": True,
                    "selected_tiers": {"economy": 1},
                    "static_tiers": {"economy": 1},
                    "cost_micro_usd": 20,
                    "latency_ms": 40,
                    "guard_promotions": 0,
                    "excluded_non_task_errors": 0,
                    "excluded_error_rule_ids": [],
                    "attribution_ambiguity": False,
                    "recovery_dependency": False,
                    "critical_violations": None,
                    "critical_violations_known": False,
                    "associated_terminal_pass": True,
                    "associated_terminal_fail": False,
                }
            )

        result = adapter.build_experience_matrix(evidence)[0]

        self.assertEqual(result["active_recommendation"], "retain")
        self.assertFalse(result["controlled_validation_candidate"])
        self.assertEqual(result["screening_reason"], "critical_evidence_unknown")

        for row in evidence:
            row["critical_violations"] = 1
            row["critical_violations_known"] = True
        nonzero = adapter.build_experience_matrix(evidence)[0]
        self.assertEqual(nonzero["active_recommendation"], "retain")
        self.assertFalse(nonzero["controlled_validation_candidate"])
        self.assertEqual(nonzero["screening_reason"], "critical_violations")


class ProductionCoverageTests(unittest.TestCase):
    def test_request_outcome_input_and_per_request_coverage_are_required(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages = task_messages("missing-outcome", 1)
            install_trial(run_dir, "missing-outcome", messages)
            traces, decisions, _ = raw_inputs("missing-outcome", messages)
            write_jsonl(root / "traces.jsonl", traces)
            write_jsonl(root / "decisions.jsonl", decisions)
            with self.assertRaisesRegex(ValueError, "request-outcomes"):
                adapter.run(
                    run_dir,
                    root / "decisions.jsonl",
                    root / "missing-input-output",
                    root / "traces.jsonl",
                )

            output = run_fixture(root, run_dir, traces, decisions, [])
            summary = json.loads((output / "join-summary.json").read_text())
            evidence = [json.loads(line) for line in (output / "task-evidence.jsonl").read_text().splitlines()]
            self.assertIn(
                "trace_missing_request_outcome",
                summary["task_coverage_reasons"]["missing-outcome"],
            )
            self.assertTrue(evidence)
            self.assertTrue(all(row["quality_credit_eligible"] is False for row in evidence))

    def test_partial_same_task_join_blocks_surviving_request_quality(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages = task_messages("partial", 2)
            install_trial(run_dir, "partial", messages)
            traces, decisions, outcomes = raw_inputs("partial", messages)
            decisions.pop()

            output = run_fixture(root, run_dir, traces, decisions, outcomes)

            evidence = [json.loads(line) for line in (output / "task-evidence.jsonl").read_text().splitlines()]
            matrix = json.loads((output / "matrix.json").read_text())
            summary = json.loads((output / "join-summary.json").read_text())
            self.assertTrue(evidence)
            self.assertTrue(all(row["quality_credit_eligible"] is False for row in evidence))
            self.assertEqual(matrix[0]["independent_tasks"], 0)
            self.assertEqual(matrix[0]["active_recommendation"], "retain")
            self.assertIn("trace_missing_decision", summary["task_coverage_reasons"]["partial"])
            self.assertFalse(summary["global_quality_blocked"])

    def test_authoritative_request_schema_counts_all_exclusions(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages = task_messages("errors", 3)
            install_trial(run_dir, "errors", messages)
            traces, decisions, outcomes = raw_inputs(
                "errors",
                messages,
                errors=[
                    "upstream_unavailable",
                    "upstream_policy_violation",
                    "upstream_timeout",
                ],
                real_decision_schema=True,
            )

            output = run_fixture(root, run_dir, traces, decisions, outcomes)

            summary = json.loads((output / "join-summary.json").read_text())
            evidence = [json.loads(line) for line in (output / "task-evidence.jsonl").read_text().splitlines()]
            self.assertEqual(summary["excluded_non_task_errors"], 3)
            self.assertEqual(
                summary["excluded_error_categories"], {"provider": 2, "transport": 1}
            )
            self.assertEqual(sum(row["excluded_non_task_errors"] for row in evidence), 3)

    def test_task_wide_recovery_and_critical_state_reach_every_cell(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages = task_messages("recovery", 2)
            install_trial(run_dir, "recovery", messages, critical_violations=0)
            traces, decisions, outcomes = raw_inputs(
                "recovery",
                messages,
                tiers=["balanced", "strong"],
                routes=[
                    "agent_route/v1|unknown|implement|normal",
                    "agent_route/v1|unknown|verify|normal",
                ],
            )

            output = run_fixture(root, run_dir, traces, decisions, outcomes)

            evidence = [json.loads(line) for line in (output / "task-evidence.jsonl").read_text().splitlines()]
            self.assertEqual(len(evidence), 2)
            self.assertTrue(all(row["recovery_dependency"] for row in evidence))
            self.assertTrue(all(row["critical_violations_known"] for row in evidence))
            self.assertTrue(all(row["critical_violations"] == 0 for row in evidence))

    def test_request_errors_are_not_duplicated_across_task_route_cells(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages = task_messages("cell-errors", 2)
            install_trial(run_dir, "cell-errors", messages, critical_violations=0)
            traces, decisions, outcomes = raw_inputs(
                "cell-errors",
                messages,
                routes=[
                    "agent_route/v1|unknown|implement|normal",
                    "agent_route/v1|unknown|verify|normal",
                ],
                errors=["upstream_unavailable", None],
            )

            output = run_fixture(root, run_dir, traces, decisions, outcomes)

            evidence = [json.loads(line) for line in (output / "task-evidence.jsonl").read_text().splitlines()]
            self.assertEqual(sum(row["excluded_non_task_errors"] for row in evidence), 1)

    def test_duplicate_and_missing_input_identities_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages = task_messages("duplicate", 2)
            install_trial(run_dir, "duplicate", messages)
            traces, decisions, outcomes = raw_inputs("duplicate", messages)
            decisions[1]["decision_id"] = decisions[0]["decision_id"]
            with self.assertRaisesRegex(ValueError, "duplicate decision"):
                run_fixture(root, run_dir, traces, decisions, outcomes)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages = task_messages("duplicate-ingress", 2)
            install_trial(run_dir, "duplicate-ingress", messages)
            traces, decisions, outcomes = raw_inputs("duplicate-ingress", messages)
            decisions[1]["ingress_request_id_sha256"] = decisions[0]["ingress_request_id_sha256"]
            with self.assertRaisesRegex(ValueError, "duplicate decision ingress"):
                run_fixture(root, run_dir, traces, decisions, outcomes)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages = task_messages("duplicate-row", 1)
            install_trial(run_dir, "duplicate-row", messages)
            traces, decisions, outcomes = raw_inputs("duplicate-row", messages)
            outcomes.append(dict(outcomes[0]))
            with self.assertRaisesRegex(ValueError, "duplicate"):
                run_fixture(root, run_dir, traces, decisions, outcomes)

    def test_duplicate_result_rows_and_result_ids_are_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages = task_messages("result-row", 1)
            install_trial(run_dir, "result-row", messages)
            original = run_dir / "jobs" / "job" / "result-row__trial"
            duplicate = run_dir / "jobs" / "other" / "result-row__trial"
            (duplicate / "agent").mkdir(parents=True)
            (duplicate / "result.json").write_bytes((original / "result.json").read_bytes())
            (duplicate / "agent" / "trajectory.json").write_bytes(
                (original / "agent" / "trajectory.json").read_bytes()
            )
            traces, decisions, outcomes = raw_inputs("result-row", messages)
            with self.assertRaisesRegex(ValueError, "duplicate result row"):
                run_fixture(root, run_dir, traces, decisions, outcomes)

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages_a = task_messages("result-id-a", 1)
            messages_b = task_messages("result-id-b", 1)
            install_trial(run_dir, "result-id-a", messages_a, result_id="same-result")
            install_trial(run_dir, "result-id-b", messages_b, result_id="same-result")
            traces, decisions, outcomes = raw_inputs("result-id-a", messages_a)
            with self.assertRaisesRegex(ValueError, "duplicate result identity"):
                run_fixture(root, run_dir, traces, decisions, outcomes)

    def test_attempt_identity_is_stable_and_does_not_collide(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            all_traces: list[dict[str, object]] = []
            all_decisions: list[dict[str, object]] = []
            all_outcomes: list[dict[str, object]] = []
            for attempt in ("one", "two"):
                identity = f"canonical-{attempt}"
                messages = task_messages(identity, 1)
                install_trial(
                    run_dir,
                    "canonical",
                    messages,
                    trial_name=f"canonical__{attempt}",
                    result_id=f"result-{attempt}",
                )
                traces, decisions, outcomes = raw_inputs(identity, messages)
                all_traces.extend(traces)
                all_decisions.extend(decisions)
                all_outcomes.extend(outcomes)

            output = run_fixture(root, run_dir, all_traces, all_decisions, all_outcomes)
            first = (output / "packets.jsonl").read_bytes()
            packets = [json.loads(line) for line in first.splitlines()]
            self.assertEqual(len({packet["subject"]["eval_id"] for packet in packets}), 2)
            self.assertEqual(len({packet["subject"]["subject_id"] for packet in packets}), 2)
            self.assertEqual(json.loads((output / "matrix.json").read_text())[0]["independent_tasks"], 1)

            repeat = root / "repeat"
            adapter.run(
                run_dir,
                root / "decisions.jsonl",
                repeat,
                root / "traces.jsonl",
                root / "request-outcomes.jsonl",
            )
            self.assertEqual(first, (repeat / "packets.jsonl").read_bytes())

    def test_repeated_attempt_identity_is_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            run_dir = root / "run"
            messages_a = task_messages("repeat-a", 1)
            messages_b = task_messages("repeat-b", 1)
            install_trial(run_dir, "canonical-a", messages_a, trial_name="same-attempt", result_id="one")
            second = run_dir / "jobs" / "other" / "same-attempt"
            second.parent.mkdir(parents=True)
            second.mkdir()
            (second / "agent").mkdir()
            (second / "result.json").write_text(
                json.dumps({**task_result("canonical-b"), "trial_name": "same-attempt", "id": "two", "critical_violations": 0}),
                encoding="utf-8",
            )
            (second / "agent" / "trajectory.json").write_text(
                json.dumps({"steps": [{"source": "user", "message": messages_b[0]["content"]}]}),
                encoding="utf-8",
            )
            traces, decisions, outcomes = raw_inputs("repeat-a", messages_a)
            with self.assertRaisesRegex(ValueError, "duplicate trial"):
                run_fixture(root, run_dir, traces, decisions, outcomes)


class InputHardeningTests(unittest.TestCase):
    def test_request_accounting_accepts_rfc3339_nanoseconds(self) -> None:
        latency = adapter._request_latency_ms(
            {
                "decision_at": "2026-08-17T06:17:19.587771000+00:00",
                "created_at": "2026-08-17T06:17:28.227834861+00:00",
            }
        )

        self.assertEqual(latency, 8640)

    def test_jsonl_line_bound_and_csv_formula_escape(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path = root / "large.jsonl"
            path.write_text(json.dumps({"value": "x" * 64}) + "\n", encoding="utf-8")
            old_bound = adapter.MAX_JSONL_LINE_BYTES
            adapter.MAX_JSONL_LINE_BYTES = 32
            try:
                with self.assertRaisesRegex(ValueError, "line"):
                    adapter._load_jsonl(path)
            finally:
                adapter.MAX_JSONL_LINE_BYTES = old_bound
            self.assertEqual(adapter._csv_safe("=1+1"), "'=1+1")


class CliTests(unittest.TestCase):
    def test_cli_output_is_deterministic(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            fixture = root / "run"
            messages = task_messages("deterministic", 1)
            install_trial(fixture, "deterministic", messages)
            traces, decisions, outcomes = raw_inputs("deterministic", messages)
            write_jsonl(root / "traces.jsonl", traces)
            write_jsonl(root / "decisions.jsonl", decisions)
            write_jsonl(root / "outcomes.jsonl", outcomes)
            left = root / "left"
            right = root / "right"
            adapter.run(fixture, root / "decisions.jsonl", left, root / "traces.jsonl", root / "outcomes.jsonl")
            adapter.run(fixture, root / "decisions.jsonl", right, root / "traces.jsonl", root / "outcomes.jsonl")

            names = [
                "packets.jsonl",
                "task-evidence.jsonl",
                "matrix.json",
                "matrix.csv",
                "join-summary.json",
            ]
            for name in names:
                with self.subTest(name=name):
                    self.assertEqual(
                        (left / name).read_bytes(),
                        (right / name).read_bytes(),
                    )
            matrix = json.loads((left / "matrix.json").read_text())
            self.assertIn("controlled_validation_candidate", matrix[0])
            self.assertIn("screening_reason", matrix[0])

            binary = REPO_ROOT / "target" / "debug" / "bitrouter"
            if not binary.exists():
                subprocess.run(
                    ["cargo", "build", "-p", "bitrouter", "--bin", "bitrouter"],
                    cwd=REPO_ROOT,
                    check=True,
                )
            validation = root / "validation"
            validation.mkdir()
            config = validation / "bitrouter.yaml"
            config.write_text(
                "inherit_defaults: false\ndatabase:\n  url: sqlite://./eval.db\n",
                encoding="utf-8",
            )
            for index, packet in enumerate(
                json.loads(line) for line in (left / "packets.jsonl").read_text().splitlines()
            ):
                draft = validation / f"subject-{index}.json"
                sealed = validation / f"sealed-{index}.json"
                result = validation / f"result-{index}.json"
                draft.write_text(json.dumps(packet["subject"]), encoding="utf-8")
                result.write_text(json.dumps(packet["result"]), encoding="utf-8")
                subprocess.run(
                    [str(binary), "eval", "subject", "seal", str(draft), "--output", str(sealed)],
                    cwd=validation,
                    check=True,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(
                    json.loads(sealed.read_text())["evidence_digest"],
                    packet["subject"]["evidence_digest"],
                )
                subprocess.run(
                    [str(binary), "eval", "subject", "put", str(sealed), "--config", str(config)],
                    cwd=validation,
                    check=True,
                    capture_output=True,
                    text=True,
                )
                subprocess.run(
                    [str(binary), "eval", "result", "submit", str(result), "--config", str(config)],
                    cwd=validation,
                    check=True,
                    capture_output=True,
                    text=True,
                )


if __name__ == "__main__":
    unittest.main()
