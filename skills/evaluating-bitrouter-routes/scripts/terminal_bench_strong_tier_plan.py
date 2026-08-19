#!/usr/bin/env python3
"""Plan a strong-tier route promotion from fully clean Terminal-Bench evidence."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import re
from collections import Counter, defaultdict
from decimal import Decimal, InvalidOperation, ROUND_HALF_UP
from pathlib import Path


SCHEMA_VERSION = 1
MILLION = Decimal(1_000_000)
NINE_PLACES = Decimal("0.000000001")
ANSI = re.compile(r"\x1b\[[0-9;]*m")
REQUEST_ID = re.compile(r'request_id="([^"]+)"')


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--validity-audit", type=Path, required=True)
    parser.add_argument("--request-join", type=Path, required=True)
    parser.add_argument("--daemon-log", type=Path, required=True)
    parser.add_argument(
        "--control-attempt-cost",
        action="append",
        required=True,
        help="Nominal USD cost for one exact-case control attempt; repeat per attempt.",
    )
    parser.add_argument(
        "--control-anchor",
        choices=("cheapest",),
        required=True,
        help="Explicit operator-approved rule for selecting the target-band anchor.",
    )
    parser.add_argument("--target-policy-key", required=True)
    parser.add_argument(
        "--strong-rates",
        required=True,
        help="USD per million uncached,cache-read,cache-write,completion tokens.",
    )
    parser.add_argument("--target-savings-min-percent", default="40")
    parser.add_argument("--target-savings-max-percent", default="50")
    parser.add_argument("--output-dir", type=Path, required=True)
    return parser.parse_args()


def decimal_value(raw: object, context: str) -> Decimal:
    try:
        value = Decimal(str(raw))
    except (InvalidOperation, ValueError) as error:
        raise ValueError(f"{context} must be a decimal") from error
    if not value.is_finite():
        raise ValueError(f"{context} must be finite")
    return value


def required_nonnegative_decimal(
    row: dict[str, object], field: str
) -> Decimal:
    if field not in row or row[field] is None:
        raise ValueError(f"missing {field}")
    value = decimal_value(row[field], field)
    if value < 0:
        raise ValueError(f"{field} must be non-negative")
    return value


def money(value: Decimal) -> str:
    return str(value.quantize(NINE_PLACES, rounding=ROUND_HALF_UP))


def ppm(numerator: Decimal, denominator: Decimal) -> int:
    if denominator <= 0:
        raise ValueError("ratio denominator must be positive")
    return int(
        (numerator * Decimal(1_000_000) / denominator).quantize(
            Decimal(1), rounding=ROUND_HALF_UP
        )
    )


def load_jsonl(path: Path) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip():
            continue
        value = json.loads(line)
        if not isinstance(value, dict):
            raise ValueError(f"{path}:{line_number} must contain a JSON object")
        rows.append(value)
    return rows


def log_field(line: str, field: str) -> str | None:
    match = re.search(rf"{re.escape(field)}=(?:Some\(\")?([^\s\")]+)", line)
    return match.group(1) if match else None


def load_decisions(path: Path, request_ids: set[str]) -> dict[str, dict[str, str]]:
    decisions: dict[str, dict[str, str]] = {}
    for raw in path.read_text(encoding="utf-8", errors="replace").splitlines():
        line = ANSI.sub("", raw)
        if "policy routing decision" not in line:
            continue
        match = REQUEST_ID.search(line)
        if match is None or match.group(1) not in request_ids:
            continue
        request_id = match.group(1)
        if request_id in decisions:
            raise ValueError(f"duplicate policy decision for {request_id}")
        decision = {
            "request_key": log_field(line, "request_key") or "",
            "static_tier": log_field(line, "static_tier") or "",
            "selected_tier": log_field(line, "selected_tier") or "",
        }
        if not all(decision.values()):
            raise ValueError(f"incomplete policy decision for {request_id}")
        decisions[request_id] = decision
    return decisions


def strong_cost(row: dict[str, object], rates: tuple[Decimal, ...]) -> Decimal:
    token_fields = (
        "uncached_input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "completion_tokens",
    )
    tokens = [required_nonnegative_decimal(row, field) for field in token_fields]
    return sum(token * rate for token, rate in zip(tokens, rates)) / MILLION


def json_bytes(value: object) -> bytes:
    return (
        json.dumps(value, indent=2, sort_keys=True, ensure_ascii=False, allow_nan=False)
        + "\n"
    ).encode("utf-8")


def sha256(path: Path) -> str:
    return "sha256:" + hashlib.sha256(path.read_bytes()).hexdigest()


def write_report(summary: dict[str, object]) -> str:
    current = summary["current"]
    candidate = summary["candidate"]
    if not isinstance(current, dict) or not isinstance(candidate, dict):
        raise ValueError("summary cost sections must be objects")
    lines = [
        "# Strong-tier cost-budget plan",
        "",
        "This is a token-preserving counterfactual using only fully clean tasks.",
        "It is a nominal cost estimate, not a causal reward claim or settled invoice.",
        "",
        f"- Strict tasks: {summary['strict_task_count']}",
        f"- Exact request/decision joins: {summary['request_count']}",
        f"- Target policy key: `{summary['target_policy_key']}`",
        f"- Promoted requests: {summary['promoted_request_count']}",
        f"- Current strong share: {current['strong_request_share_ppm'] / 10_000:.2f}%",
        f"- Candidate strong share: {candidate['strong_request_share_ppm'] / 10_000:.2f}%",
        f"- Current nominal cost: ${current['nominal_cost_usd']}",
        f"- Candidate nominal cost: ${candidate['nominal_cost_usd']}",
        "",
        "## Exact-case controls",
        "",
        "Control attempts remain separate; no aggregate control value is used.",
        "",
        "| Control | Nominal cost | Candidate savings |",
        "|---|---:|---:|",
    ]
    controls = summary["controls"]
    if not isinstance(controls, list):
        raise ValueError("summary controls must be a list")
    for control in controls:
        if not isinstance(control, dict):
            raise ValueError("control row must be an object")
        lines.append(
            f"| Control attempt {control['attempt']} | "
            f"${control['nominal_cost_usd']} | {control['savings_ppm'] / 10_000:.2f}% |"
        )
    anchor = summary["conservative_anchor"]
    if not isinstance(anchor, dict):
        raise ValueError("summary conservative anchor must be an object")
    lines.extend(
        [
            "",
            "The cheapest exact-case control attempt is the conservative planning anchor: "
            f"attempt {anchor['attempt']}, with {anchor['savings_ppm'] / 10_000:.2f}% "
            "estimated savings.",
            "",
        ]
    )
    return "\n".join(lines)


def run(args: argparse.Namespace) -> None:
    validity = json.loads(args.validity_audit.read_text(encoding="utf-8"))
    strict_raw = validity.get("fully_clean_tasks") if isinstance(validity, dict) else None
    if not isinstance(strict_raw, list) or not strict_raw:
        raise ValueError("validity audit must contain non-empty fully_clean_tasks")
    strict_tasks = {str(task) for task in strict_raw}

    all_rows = load_jsonl(args.request_join)
    rows = [row for row in all_rows if row.get("task_name") in strict_tasks]
    if not rows:
        raise ValueError("strict cohort has no request rows")
    present_tasks = {str(row.get("task_name")) for row in rows}
    missing_tasks = strict_tasks - present_tasks
    if missing_tasks:
        raise ValueError(
            "strict cohort has no requests for task: " + ", ".join(sorted(missing_tasks))
        )
    request_ids: set[str] = set()
    for row in rows:
        request_id_raw = row.get("trajectory_request_id")
        if not isinstance(request_id_raw, str) or not request_id_raw:
            raise ValueError("strict cohort request missing trajectory_request_id")
        if request_id_raw in request_ids:
            raise ValueError(f"duplicate strict request row for {request_id_raw}")
        request_ids.add(request_id_raw)
        if "error" not in row:
            raise ValueError(
                f"strict cohort request missing explicit error field {request_id_raw}"
            )
        if row.get("error") is not None:
            raise ValueError(f"strict cohort contains errored request {request_id_raw}")
        if row.get("included_in_nominal_cost") is not True:
            raise ValueError(f"strict cohort contains unpriced request {request_id_raw}")
        if row.get("usage_origin") != "provider_reported":
            raise ValueError(f"strict cohort contains non-provider usage {request_id_raw}")
        required_nonnegative_decimal(row, "nominal_cost_usd")
        for token_field in (
            "uncached_input_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "completion_tokens",
        ):
            required_nonnegative_decimal(row, token_field)
    decisions = load_decisions(args.daemon_log, request_ids)
    missing_decisions = request_ids - set(decisions)
    if missing_decisions:
        raise ValueError(
            "missing policy decision for " + ", ".join(sorted(missing_decisions))
        )

    rates_raw = args.strong_rates.split(",")
    if len(rates_raw) != 4:
        raise ValueError("strong rates must contain four comma-separated values")
    rates = tuple(decimal_value(rate, "strong rate") for rate in rates_raw)
    if any(rate < 0 for rate in rates):
        raise ValueError("strong rates must be non-negative")
    control_costs = [
        decimal_value(cost, "control attempt cost") for cost in args.control_attempt_cost
    ]
    if any(cost <= 0 for cost in control_costs):
        raise ValueError("control attempt costs must be positive")
    target_min = decimal_value(args.target_savings_min_percent, "minimum savings")
    target_max = decimal_value(args.target_savings_max_percent, "maximum savings")
    if target_min < 0 or target_max > 100 or target_min > target_max:
        raise ValueError("target savings range must satisfy 0 <= min <= max <= 100")

    target_decisions = [
        decisions[str(row["trajectory_request_id"])]
        for row in rows
        if decisions[str(row["trajectory_request_id"])]["request_key"]
        == args.target_policy_key
    ]
    if not target_decisions:
        raise ValueError("target route has no observed requests")
    if any(
        decision["static_tier"] != "balanced"
        or decision["selected_tier"] != "balanced"
        for decision in target_decisions
    ):
        raise ValueError("target route must be balanced before promotion")

    route_cells: dict[str, list[tuple[dict[str, object], dict[str, str]]]] = defaultdict(list)
    current_cost = Decimal(0)
    candidate_cost = Decimal(0)
    current_strong = 0
    promoted = 0
    for row in rows:
        request_id = str(row.get("trajectory_request_id"))
        decision = decisions[request_id]
        route_cells[decision["request_key"]].append((row, decision))
        observed = required_nonnegative_decimal(row, "nominal_cost_usd")
        current_cost += observed
        current_strong += int(decision["selected_tier"] == "strong")
        if (
            decision["request_key"] == args.target_policy_key
            and decision["static_tier"] == "balanced"
            and decision["selected_tier"] == "balanced"
        ):
            candidate_cost += strong_cost(row, rates)
            promoted += 1
        else:
            candidate_cost += observed

    request_count = len(rows)
    task_count = len(strict_tasks)
    candidate_strong = current_strong + promoted
    controls: list[dict[str, object]] = []
    for attempt, cost in enumerate(control_costs, 1):
        savings = ppm(cost - candidate_cost, cost)
        controls.append(
            {
                "attempt": attempt,
                "nominal_cost_usd": money(cost),
                "savings_ppm": savings,
                "within_target": int(target_min * 10_000)
                <= savings
                <= int(target_max * 10_000),
            }
        )
    anchor = controls[
        min(range(len(control_costs)), key=lambda index: control_costs[index])
    ]
    summary: dict[str, object] = {
        "schema_version": SCHEMA_VERSION,
        "strict_task_count": task_count,
        "request_count": request_count,
        "join_failures": 0,
        "target_policy_key": args.target_policy_key,
        "promoted_request_count": promoted,
        "strong_rates_usd_per_million": [money(rate) for rate in rates],
        "target_savings_range_ppm": {
            "min": int(target_min * 10_000),
            "max": int(target_max * 10_000),
        },
        "current": {
            "cost_per_task_usd": money(current_cost / Decimal(task_count)),
            "nominal_cost_usd": money(current_cost),
            "strong_request_share_ppm": ppm(Decimal(current_strong), Decimal(request_count)),
            "strong_requests": current_strong,
        },
        "candidate": {
            "cost_per_task_usd": money(candidate_cost / Decimal(task_count)),
            "nominal_cost_usd": money(candidate_cost),
            "strong_request_share_ppm": ppm(
                Decimal(candidate_strong), Decimal(request_count)
            ),
            "strong_requests": candidate_strong,
        },
        "controls": controls,
        "control_anchor_policy": args.control_anchor,
        "conservative_anchor": anchor,
        "method": {
            "quality_claim": "none",
            "token_assumption": "observed token categories held fixed",
            "control_aggregation": "none",
            "non_task_errors": "strict cohort rejects every errored request",
        },
        "input_sha256": {
            "validity_audit": sha256(args.validity_audit),
            "request_join": sha256(args.request_join),
            "daemon_log": sha256(args.daemon_log),
        },
    }

    args.output_dir.mkdir(parents=True, exist_ok=True)
    summary_path = args.output_dir / "summary.json"
    summary_path.write_bytes(json_bytes(summary))

    csv_path = args.output_dir / "route-cells.csv"
    with csv_path.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.DictWriter(
            handle,
            fieldnames=(
                "request_key",
                "requests",
                "tasks",
                "selected_tiers",
                "current_nominal_cost_usd",
                "candidate_nominal_cost_usd",
                "promoted_requests",
            ),
        )
        writer.writeheader()
        for key in sorted(route_cells):
            entries = route_cells[key]
            old = sum(
                required_nonnegative_decimal(row, "nominal_cost_usd")
                for row, _ in entries
            )
            promote_entries = [
                (row, decision)
                for row, decision in entries
                if key == args.target_policy_key
                and decision["static_tier"] == "balanced"
                and decision["selected_tier"] == "balanced"
            ]
            replacement = sum(strong_cost(row, rates) for row, _ in promote_entries)
            kept = sum(
                required_nonnegative_decimal(row, "nominal_cost_usd")
                for row, decision in entries
                if (row, decision) not in promote_entries
            )
            tiers = Counter(decision["selected_tier"] for _, decision in entries)
            writer.writerow(
                {
                    "request_key": key,
                    "requests": len(entries),
                    "tasks": len({str(row.get("task_name")) for row, _ in entries}),
                    "selected_tiers": json.dumps(dict(sorted(tiers.items())), sort_keys=True),
                    "current_nominal_cost_usd": money(old),
                    "candidate_nominal_cost_usd": money(kept + replacement),
                    "promoted_requests": len(promote_entries),
                }
            )

    report_path = args.output_dir / "report.md"
    report_path.write_text(write_report(summary), encoding="utf-8")
    manifest = {
        name: sha256(args.output_dir / name)
        for name in ("report.md", "route-cells.csv", "summary.json")
    }
    (args.output_dir / "sha256-manifest.json").write_bytes(json_bytes(manifest))


def main() -> None:
    parser = argparse.ArgumentParser(add_help=False)
    try:
        run(parse_args())
    except (OSError, ValueError, KeyError, json.JSONDecodeError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()
