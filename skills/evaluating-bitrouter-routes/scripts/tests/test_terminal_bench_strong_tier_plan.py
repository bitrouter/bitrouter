from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).resolve().parents[1] / "terminal_bench_strong_tier_plan.py"
TARGET = "agent_route/v1|unknown|mechanical|guarded"
OTHER = "agent_route/v1|unknown|implement|normal"


def write_jsonl(path: Path, rows: list[dict[str, object]]) -> None:
    path.write_text(
        "".join(json.dumps(row, sort_keys=True) + "\n" for row in rows),
        encoding="utf-8",
    )


def request(
    request_id: str,
    task: str,
    *,
    selected_model: str,
    nominal_cost_usd: str,
    uncached: int,
    cache_read: int,
    completion: int,
    error: str | None = None,
) -> dict[str, object]:
    return {
        "trajectory_request_id": request_id,
        "task_name": f"terminal-bench/{task}",
        "selected_model": selected_model,
        "nominal_cost_usd": nominal_cost_usd,
        "uncached_input_tokens": uncached,
        "cache_read_tokens": cache_read,
        "cache_write_tokens": 0,
        "completion_tokens": completion,
        "visible_output_tokens": completion,
        "reasoning_tokens": 0,
        "usage_origin": "provider_reported",
        "included_in_nominal_cost": error is None,
        "error": error,
    }


def decision_line(
    request_id: str,
    key: str,
    *,
    static_tier: str,
    selected_tier: str,
) -> str:
    return (
        "\x1b[32mINFO\x1b[0m policy routing decision "
        f'request_id="{request_id}" request_key={key} '
        f'static_tier=Some("{static_tier}") '
        f'selected_tier=Some("{selected_tier}") reason=static_table\n'
    )


class StrongTierPlannerCliTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tempdir = tempfile.TemporaryDirectory()
        self.root = Path(self.tempdir.name)
        self.validity = self.root / "validity.json"
        self.join = self.root / "join.jsonl"
        self.log = self.root / "daemon.log"
        self.validity.write_text(
            json.dumps(
                {
                    "fully_clean_tasks": [
                        "terminal-bench/task-a",
                        "terminal-bench/task-b",
                    ]
                }
            ),
            encoding="utf-8",
        )
        write_jsonl(
            self.join,
            [
                request(
                    "request-a-target",
                    "task-a",
                    selected_model="bitrouter:deepseek/deepseek-v4-pro-0813",
                    nominal_cost_usd="0.100000000",
                    uncached=100_000,
                    cache_read=200_000,
                    completion=10_000,
                ),
                request(
                    "request-a-other",
                    "task-a",
                    selected_model="bitrouter:deepseek/deepseek-v4-pro-0813",
                    nominal_cost_usd="0.200000000",
                    uncached=10_000,
                    cache_read=20_000,
                    completion=1_000,
                ),
                request(
                    "request-b-strong",
                    "task-b",
                    selected_model="openai-codex:gpt-5.6-sol",
                    nominal_cost_usd="0.300000000",
                    uncached=20_000,
                    cache_read=40_000,
                    completion=6_000,
                ),
            ],
        )
        self.log.write_text(
            decision_line(
                "request-a-target",
                TARGET,
                static_tier="balanced",
                selected_tier="balanced",
            )
            + decision_line(
                "request-a-other",
                OTHER,
                static_tier="balanced",
                selected_tier="balanced",
            )
            + decision_line(
                "request-b-strong",
                "agent_route/v1|unknown|orchestrate|normal",
                static_tier="strong",
                selected_tier="strong",
            ),
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        self.tempdir.cleanup()

    def run_planner(
        self,
        output: Path,
        *,
        control_costs: tuple[str, ...] = (
            "2.500000000",
            "3.000000000",
            "2.800000000",
        ),
    ) -> subprocess.CompletedProcess[str]:
        control_args = [
            value
            for cost in control_costs
            for value in ("--control-attempt-cost", cost)
        ]
        return subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--validity-audit",
                str(self.validity),
                "--request-join",
                str(self.join),
                "--daemon-log",
                str(self.log),
                *control_args,
                "--control-anchor",
                "cheapest",
                "--target-policy-key",
                TARGET,
                "--strong-rates",
                "5,0.5,6.25,30",
                "--target-savings-min-percent",
                "40",
                "--target-savings-max-percent",
                "50",
                "--output-dir",
                str(output),
            ],
            check=False,
            capture_output=True,
            text=True,
        )

    def test_exact_join_reprices_only_target_and_keeps_controls_separate(self) -> None:
        output = self.root / "output"

        completed = self.run_planner(output)

        self.assertEqual(completed.returncode, 0, completed.stderr)
        summary = json.loads((output / "summary.json").read_text())
        self.assertEqual(summary["strict_task_count"], 2)
        self.assertEqual(summary["request_count"], 3)
        self.assertEqual(summary["join_failures"], 0)
        self.assertEqual(summary["promoted_request_count"], 1)
        self.assertEqual(
            summary["current"],
            {
                "cost_per_task_usd": "0.300000000",
                "nominal_cost_usd": "0.600000000",
                "strong_request_share_ppm": 333_333,
                "strong_requests": 1,
            },
        )
        self.assertEqual(
            summary["candidate"],
            {
                "cost_per_task_usd": "0.700000000",
                "nominal_cost_usd": "1.400000000",
                "strong_request_share_ppm": 666_667,
                "strong_requests": 2,
            },
        )
        self.assertEqual(
            [control["nominal_cost_usd"] for control in summary["controls"]],
            ["2.500000000", "3.000000000", "2.800000000"],
        )
        self.assertEqual(summary["conservative_anchor"]["attempt"], 1)
        self.assertEqual(summary["control_anchor_policy"], "cheapest")
        self.assertEqual(summary["conservative_anchor"]["savings_ppm"], 440_000)
        self.assertTrue(summary["conservative_anchor"]["within_target"])
        report = (output / "report.md").read_text()
        self.assertIn("token-preserving counterfactual", report)
        self.assertIn("Control attempt 1", report)
        self.assertIn("Control attempt 2", report)
        self.assertIn("Control attempt 3", report)
        self.assertNotIn("average control", report.lower())

    def test_outputs_are_byte_deterministic(self) -> None:
        first = self.root / "first"
        second = self.root / "second"
        one = self.run_planner(first)
        two = self.run_planner(second)

        self.assertEqual(one.returncode, 0, one.stderr)
        self.assertEqual(two.returncode, 0, two.stderr)
        for name in ("summary.json", "route-cells.csv", "report.md", "sha256-manifest.json"):
            self.assertEqual((first / name).read_bytes(), (second / name).read_bytes())

    def test_missing_policy_decision_fails_closed(self) -> None:
        lines = self.log.read_text().splitlines(keepends=True)
        self.log.write_text("".join(lines[:-1]), encoding="utf-8")

        completed = self.run_planner(self.root / "missing")

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("missing policy decision", completed.stderr)

    def test_duplicate_policy_decision_fails_closed(self) -> None:
        duplicate = decision_line(
            "request-a-target",
            TARGET,
            static_tier="balanced",
            selected_tier="balanced",
        )
        self.log.write_text(self.log.read_text() + duplicate, encoding="utf-8")

        completed = self.run_planner(self.root / "duplicate")

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("duplicate policy decision", completed.stderr)

    def test_errored_request_in_strict_cohort_fails_closed(self) -> None:
        rows = [json.loads(line) for line in self.join.read_text().splitlines()]
        rows[0]["error"] = "upstream_unavailable"
        rows[0]["included_in_nominal_cost"] = False
        write_jsonl(self.join, rows)

        completed = self.run_planner(self.root / "errored")

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("strict cohort contains errored request", completed.stderr)

    def test_missing_error_field_fails_closed(self) -> None:
        rows = [json.loads(line) for line in self.join.read_text().splitlines()]
        del rows[0]["error"]
        write_jsonl(self.join, rows)

        completed = self.run_planner(self.root / "missing-error")

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("missing explicit error field", completed.stderr)

    def test_missing_or_negative_cost_evidence_fails_closed(self) -> None:
        original_rows = [
            json.loads(line) for line in self.join.read_text().splitlines()
        ]
        for mutation, expected in (
            (("uncached_input_tokens", None), "missing uncached_input_tokens"),
            (("cache_read_tokens", -1), "cache_read_tokens must be non-negative"),
            (("nominal_cost_usd", None), "missing nominal_cost_usd"),
            (("nominal_cost_usd", "-0.1"), "nominal_cost_usd must be non-negative"),
        ):
            with self.subTest(mutation=mutation):
                rows = json.loads(json.dumps(original_rows))
                field, value = mutation
                if value is None:
                    del rows[0][field]
                else:
                    rows[0][field] = value
                write_jsonl(self.join, rows)

                completed = self.run_planner(self.root / f"invalid-{field}")

                self.assertNotEqual(completed.returncode, 0)
                self.assertIn(expected, completed.stderr)

    def test_target_must_be_an_observed_balanced_to_balanced_cell(self) -> None:
        self.log.write_text(
            self.log.read_text().replace(
                'static_tier=Some("balanced") selected_tier=Some("balanced")',
                'static_tier=Some("economy") selected_tier=Some("economy")',
                1,
            ),
            encoding="utf-8",
        )

        completed = self.run_planner(self.root / "wrong-tier")

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("target route must be balanced before promotion", completed.stderr)

    def test_target_must_have_at_least_one_observed_request(self) -> None:
        self.log.write_text(self.log.read_text().replace(TARGET, OTHER), encoding="utf-8")

        completed = self.run_planner(self.root / "absent-target")

        self.assertNotEqual(completed.returncode, 0)
        self.assertIn("target route has no observed requests", completed.stderr)

    def test_cheapest_anchor_uses_unrounded_control_cost(self) -> None:
        output = self.root / "precise-anchor"

        completed = self.run_planner(
            output,
            control_costs=("2.0000000004", "2.0000000003"),
        )

        self.assertEqual(completed.returncode, 0, completed.stderr)
        summary = json.loads((output / "summary.json").read_text())
        self.assertEqual(summary["conservative_anchor"]["attempt"], 2)


if __name__ == "__main__":
    unittest.main()
