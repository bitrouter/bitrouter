from __future__ import annotations

import importlib.util
import json
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
        "task_name": f"terminal-bench/{task_id}",
        "task_checksum": task_id,
        "trial_name": f"{task_id}__trial",
        "verifier_result": verifier,
        "exception_info": exception,
        "finished_at": "2026-08-17T00:01:00Z",
    }


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


class CliTests(unittest.TestCase):
    def test_cli_output_is_deterministic(self) -> None:
        fixture = Path(__file__).resolve().parent / "fixtures" / "run"
        with tempfile.TemporaryDirectory() as left, tempfile.TemporaryDirectory() as right:
            adapter.run(fixture, fixture / "decisions.jsonl", Path(left))
            adapter.run(fixture, fixture / "decisions.jsonl", Path(right))

            names = ["packets.jsonl", "task-evidence.jsonl", "matrix.json", "matrix.csv"]
            for name in names:
                with self.subTest(name=name):
                    self.assertEqual(
                        (Path(left) / name).read_bytes(),
                        (Path(right) / name).read_bytes(),
                    )
            matrix = json.loads((Path(left) / "matrix.json").read_text())
            self.assertIn("controlled_validation_candidate", matrix[0])
            self.assertIn("screening_reason", matrix[0])


if __name__ == "__main__":
    unittest.main()
