#!/usr/bin/env python3
"""Build conservative Eval Exchange packets and route evidence from Harbor results.

Decision rows must already carry an exact, non-temporal task join. This adapter
fails closed when the exact content/ingress identity is absent or ambiguous.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import re
from collections import Counter, defaultdict
from pathlib import Path
from typing import Iterable, NamedTuple


ADAPTER_VERSION = "terminal-bench-route-evidence-v1"
TAXONOMY_VERSION = "request-error-taxonomy-v1"
QUALITY_METRIC = "quality.pass"
ROUTE_PREFIX = "agent_route/v1|"
NON_TASK_CATEGORIES = {"provider", "network", "auth", "rate_limit", "transport"}


class RequestError(NamedTuple):
    category: str | None
    rule_id: str


class Outcome(NamedTuple):
    terminal_verdict: str
    reward: int | None
    excluded_error: RequestError | None


ERROR_RULES = (
    ("auth", "auth.http-401-403.v1", lambda status, text: status in {401, 403}),
    ("rate_limit", "rate-limit.http-429.v1", lambda status, text: status == 429),
    (
        "auth",
        "auth.credentials.v1",
        lambda status, text: any(
            marker in text
            for marker in ("invalid api key", "authentication failed", "unauthorized")
        ),
    ),
    (
        "rate_limit",
        "rate-limit.marker.v1",
        lambda status, text: "rate limit" in text or "too many requests" in text,
    ),
    (
        "network",
        "network.dns.v1",
        lambda status, text: any(
            marker in text
            for marker in ("dns lookup failed", "name resolution", "no such host")
        ),
    ),
    (
        "transport",
        "transport.connection-reset.v1",
        lambda status, text: any(
            marker in text
            for marker in ("connection reset", "broken pipe", "upstream_timeout")
        ),
    ),
    (
        "provider",
        "provider.policy-violation.v1",
        lambda status, text: any(
            marker in text
            for marker in ("upstream_policy_violation", "content policy violation")
        ),
    ),
    (
        "provider",
        "provider.upstream-unavailable.v1",
        lambda status, text: any(
            marker in text
            for marker in ("upstream_unavailable", "upstream unavailable", "provider unavailable")
        ),
    ),
)


MATRIX_COLUMNS = (
    "policy",
    "route_projection",
    "task_family",
    "role",
    "risk",
    "independent_tasks",
    "independent_episodes",
    "associated_tasks",
    "pass",
    "fail",
    "inconclusive",
    "pass_rate_ppm",
    "selected_tier_distribution",
    "static_tier_distribution",
    "cost_micro_usd",
    "latency_ms_mean",
    "guard_promotions",
    "excluded_non_task_errors",
    "excluded_error_rule_ids",
    "attribution_ambiguities",
    "recovery_dependencies",
    "critical_violations",
    "evidence_grade",
    "quality_credit_eligible",
    "active_recommendation",
    "economy_experiment_candidate",
    "controlled_validation_candidate",
    "screening_reason",
)


def _json_bytes(value: object, *, sort_keys: bool = False) -> bytes:
    return json.dumps(
        value,
        ensure_ascii=False,
        separators=(",", ":"),
        sort_keys=sort_keys,
    ).encode("utf-8")


def _digest(value: bytes | object) -> str:
    raw = value if isinstance(value, bytes) else _json_bytes(value, sort_keys=True)
    return "sha256:" + hashlib.sha256(raw).hexdigest()


def ingress_commitment(request_id: str) -> str:
    domain = b"bitrouter.ingress-request-id.v1\0"
    return "sha256:" + hashlib.sha256(domain + request_id.encode("utf-8")).hexdigest()


def _message_semantics(messages: Iterable[dict[str, object]]) -> list[dict[str, object]]:
    return [
        {"role": message.get("role"), "content": message.get("content")}
        for message in messages
    ]


def _message_hash(messages: Iterable[dict[str, object]]) -> str:
    return _digest(_message_semantics(messages))


def _task_description_hash(content: object) -> str | None:
    if not isinstance(content, str):
        return None
    match = re.search(
        r"Task Description:\n(.*?)(?:\n\n(?:Current terminal state:|Current Terminal Screen:)|\Z)",
        content,
        re.S,
    )
    return _digest(match.group(1).strip()) if match else None


def join_raw_decisions(
    trial_histories: dict[str, list[dict[str, object]]],
    traces: list[dict[str, object]],
    decisions: list[dict[str, object]],
) -> tuple[list[dict[str, object]], dict[str, int]]:
    prefix_to_tasks: dict[str, set[str]] = defaultdict(set)
    description_to_tasks: dict[str, set[str]] = defaultdict(set)
    for task_id, history in trial_histories.items():
        prefix = []
        for message in history:
            prefix.append(message)
            if message.get("role") == "user":
                prefix_to_tasks[_message_hash(prefix)].add(task_id)
                description = _task_description_hash(message.get("content"))
                if description is not None:
                    description_to_tasks[description].add(task_id)

    decisions_by_ingress: dict[str, list[dict[str, object]]] = defaultdict(list)
    for row in decisions:
        commitment = row.get("ingress_request_id_sha256")
        if isinstance(commitment, str) and commitment:
            decisions_by_ingress[commitment].append(row)

    joined = []
    summary = {"joined": 0, "ambiguous": 0, "unmatched": 0}
    for trace in traces:
        raw_body = trace.get("raw_body")
        raw_messages = raw_body.get("messages") if isinstance(raw_body, dict) else None
        if not isinstance(raw_messages, list) or not all(
            isinstance(message, dict) for message in raw_messages
        ):
            summary["unmatched"] += 1
            continue
        messages = list(raw_messages)
        task_candidates = prefix_to_tasks.get(_message_hash(messages), set())
        method = "full_messages_prefix" if task_candidates else None
        if not task_candidates:
            descriptions = {
                description
                for description in (
                    _task_description_hash(message.get("content"))
                    for message in messages
                    if message.get("role") == "user"
                )
                if description is not None
            }
            described_tasks = set()
            for description in descriptions:
                described_tasks.update(description_to_tasks.get(description, set()))
            task_candidates = described_tasks
            method = "task_description_field" if task_candidates else None

        trace_id = trace.get("id")
        decision_candidates = (
            decisions_by_ingress.get(ingress_commitment(str(trace_id)), [])
            if isinstance(trace_id, str) and trace_id
            else []
        )
        if len(task_candidates) == 1 and len(decision_candidates) == 1 and method:
            row = dict(decision_candidates[0])
            row["exact_task_id"] = next(iter(task_candidates))
            row["task_join_method"] = method
            row["exact_ingress_join"] = True
            joined.append(row)
            summary["joined"] += 1
        elif len(task_candidates) > 1 or len(decision_candidates) > 1:
            summary["ambiguous"] += 1
        else:
            summary["unmatched"] += 1
    return joined, summary


def classify_request_error(raw: dict[str, object]) -> RequestError:
    status_raw = raw.get("status", raw.get("http_status"))
    status = status_raw if isinstance(status_raw, int) and not isinstance(status_raw, bool) else None
    text = " ".join(
        str(raw.get(field, ""))
        for field in ("error", "code", "type", "exception_type", "exception_message")
    ).lower()
    for category, rule_id, predicate in ERROR_RULES:
        if predicate(status, text):
            return RequestError(category, rule_id)
    return RequestError(None, "unclassified.v1")


def classify_outcome(raw: dict[str, object]) -> Outcome:
    verifier = raw.get("verifier_result")
    rewards = verifier.get("rewards") if isinstance(verifier, dict) else None
    reward_raw = rewards.get("reward") if isinstance(rewards, dict) else None
    if isinstance(reward_raw, (int, float)) and not isinstance(reward_raw, bool):
        reward = float(reward_raw)
        if reward == 1.0:
            return Outcome("pass", 1, None)
        if reward == 0.0:
            return Outcome("fail", 0, None)

    exception = raw.get("exception_info")
    if isinstance(exception, dict):
        classified = classify_request_error(exception)
        if classified.category in NON_TASK_CATEGORIES:
            return Outcome("inconclusive", None, classified)
    return Outcome("inconclusive", None, None)


def _decision_ref(raw: dict[str, object]) -> dict[str, object]:
    reference: dict[str, object] = {
        "decision_id": str(raw["decision_id"]),
        "policy": str(raw["policy"]),
        "route_projection": str(raw["route_projection"]),
        "request_key": str(raw["request_key"]),
        "selected_tier": str(raw["selected_tier"]),
    }
    if raw.get("selected_effort") is not None:
        reference["selected_effort"] = raw["selected_effort"]
    reference["baseline_tier"] = raw.get("baseline_tier")
    if raw.get("baseline_effort") is not None:
        reference["baseline_effort"] = raw["baseline_effort"]
    reference["policy_digest"] = str(raw["policy_digest"])
    return reference


def _task_identity(raw: dict[str, object]) -> str:
    exact = raw.get("exact_task_id")
    if isinstance(exact, str) and exact:
        return exact
    checksum = raw.get("task_checksum")
    if isinstance(checksum, str) and checksum:
        return checksum
    task_name = raw.get("task_name")
    return str(task_name) if task_name else "unidentified-task"


def _valid_exact_decision(raw: dict[str, object]) -> bool:
    return (
        raw.get("exact_ingress_join") is True
        and raw.get("task_join_method")
        in {"full_messages_prefix", "task_description_field"}
        and all(
            isinstance(raw.get(field), str) and bool(raw.get(field))
            for field in (
                "decision_id",
                "policy",
                "policy_digest",
                "route_projection",
                "request_key",
                "selected_tier",
                "captured_at",
            )
        )
    )


def _attribution(
    decisions: list[dict[str, object]],
    outcome: Outcome,
    errors: list[RequestError],
) -> tuple[bool, str, str | None]:
    if outcome.terminal_verdict not in {"pass", "fail"}:
        return False, "terminal_outcome_inconclusive", None
    if not decisions or not all(_valid_exact_decision(row) for row in decisions):
        return False, "exact_decision_join_missing", None
    if len({str(row["policy"]) for row in decisions}) != 1:
        return False, "multiple_policies", None
    if len({str(row["route_projection"]) for row in decisions}) != 1:
        return False, "multiple_route_cells", None
    if len({str(row["selected_tier"]) for row in decisions}) != 1:
        return False, "multiple_selected_tiers", None
    if errors:
        representative = min(
            decisions,
            key=lambda row: (str(row["captured_at"]), str(row["decision_id"])),
        )
        return False, "non_task_error_contamination", str(representative["decision_id"])
    representative = min(
        decisions,
        key=lambda row: (str(row["captured_at"]), str(row["decision_id"])),
    )
    return True, "unique_route_cell_and_tier", str(representative["decision_id"])


def _evidence_items(
    raw: dict[str, object], outcome: Outcome, attribution_reason: str
) -> list[dict[str, object]]:
    reward = "none" if outcome.reward is None else str(outcome.reward)
    items = [
        {
            "evidence_id": "classification",
            "kind": "task.classification",
            "digest": _digest(
                {
                    "adapter_version": ADAPTER_VERSION,
                    "taxonomy_version": TAXONOMY_VERSION,
                    "terminal_verdict": outcome.terminal_verdict,
                    "attribution_reason": attribution_reason,
                }
            ),
            "redacted": True,
            "attributes": {
                "attribution_reason": attribution_reason,
                "terminal_verdict": outcome.terminal_verdict,
            },
        },
        {
            "evidence_id": "verifier",
            "kind": "task.verifier",
            "digest": _digest(raw),
            "redacted": True,
            "attributes": {"reward": reward},
        },
    ]
    return sorted(items, key=lambda item: str(item["evidence_id"]))


def build_packet(
    raw: dict[str, object],
    decisions: list[dict[str, object]],
    request_errors: list[RequestError],
) -> tuple[dict[str, object], dict[str, object]]:
    outcome = classify_outcome(raw)
    errors = [error for error in request_errors if error.category in NON_TASK_CATEGORIES]
    if not errors and outcome.excluded_error is not None:
        errors = [outcome.excluded_error]
    eligible, reason, representative = _attribution(decisions, outcome, errors)
    copied_decisions = [_decision_ref(row) for row in decisions if _valid_exact_decision(row)]
    policy_digest = str(copied_decisions[0]["policy_digest"]) if copied_decisions else ""
    task_id = _task_identity(raw)
    eval_id = "tb-route-" + hashlib.sha256(task_id.encode("utf-8")).hexdigest()[:32]
    evidence = _evidence_items(raw, outcome, reason)
    evidence_digest = _digest(_json_bytes(evidence))
    packet_verdict = outcome.terminal_verdict if eligible or errors else "inconclusive"
    metrics: dict[str, object] = {}
    if packet_verdict in {"pass", "fail"}:
        metrics[QUALITY_METRIC] = {
            "value": int(packet_verdict == "pass"),
            "unit": "boolean",
        }
    decision_credit: dict[str, object] = {}
    if eligible and representative is not None:
        decision_credit[representative] = {
            "weight_ppm": 1_000_000,
            "metric_ids": [QUALITY_METRIC],
        }
    elif errors and representative is not None and packet_verdict in {"pass", "fail"}:
        decision_credit[representative] = {
            "weight_ppm": 0,
            "metric_ids": [QUALITY_METRIC],
        }
    observed_at = str(raw.get("finished_at") or "1970-01-01T00:00:00Z")
    subject = {
        "schema_version": 1,
        "eval_id": eval_id,
        "scope": "task",
        "subject_id": task_id,
        "policy_digest": policy_digest,
        "preset": "auto",
        "cohort": "terminal-bench-observational",
        "holdout": False,
        "decisions": copied_decisions,
        "requested_dimensions": sorted(metrics),
        "evidence": evidence,
        "evidence_digest": evidence_digest,
        "observed_at": observed_at,
    }
    config_digest = _digest(
        {"adapter": ADAPTER_VERSION, "taxonomy": TAXONOMY_VERSION}
    )
    result = {
        "schema_version": 1,
        "eval_id": eval_id,
        "evidence_digest": evidence_digest,
        "evaluator": {
            "authority_id": "local",
            "evaluator_id": "terminal-bench-adapter",
            "kind": "task_native",
            "version": "1",
            "config_digest": config_digest,
        },
        "verdict": packet_verdict,
        "metrics": metrics,
        "hard_violations": [],
        "confidence_ppm": 1_000_000 if outcome.reward is not None else None,
        "evidence_refs": ["classification", "verifier"],
        "decision_credit": decision_credit,
        "idempotency_key": "tb-route-" + _digest(raw).split(":", 1)[1][:32],
        "submitted_at": observed_at,
    }
    route_projections = sorted(
        {str(row["route_projection"]) for row in decisions if row.get("route_projection")}
    )
    selected_tiers = Counter(
        str(row["selected_tier"]) for row in decisions if row.get("selected_tier")
    )
    static_tiers = Counter(
        str(row["static_tier"]) for row in decisions if row.get("static_tier")
    )
    summary: dict[str, object] = {
        "task_id": task_id,
        "policy": str(decisions[0].get("policy", "")) if decisions else "",
        "route_projection": route_projections[0] if len(route_projections) == 1 else "",
        "terminal_verdict": outcome.terminal_verdict,
        "quality_credit_eligible": eligible,
        "representative_decision_id": representative,
        "attribution_reason": reason,
        "selected_tiers": dict(sorted(selected_tiers.items())),
        "static_tiers": dict(sorted(static_tiers.items())),
        "cost_micro_usd": sum(
            int(row.get("cost_micro_usd", 0) or 0) for row in decisions
        ),
        "latency_ms": sum(int(row.get("latency_ms", 0) or 0) for row in decisions),
        "guard_promotions": sum(
            bool(row.get("progress_clause_ids"))
            or row.get("selected_tier") != row.get("static_tier")
            for row in decisions
        ),
        "excluded_non_task_errors": len(errors),
        "excluded_error_rule_ids": sorted(error.rule_id for error in errors),
        "attribution_ambiguity": reason
        in {"multiple_policies", "multiple_route_cells", "multiple_selected_tiers"},
        "recovery_dependency": _has_recovery_dependency(decisions),
        "critical_violations": 0,
        "associated_terminal_pass": outcome.terminal_verdict == "pass",
        "associated_terminal_fail": outcome.terminal_verdict == "fail",
    }
    return {"subject": subject, "result": result}, summary


def _has_recovery_dependency(decisions: list[dict[str, object]]) -> bool:
    ordered = sorted(
        decisions,
        key=lambda row: (str(row.get("captured_at", "")), str(row.get("decision_id", ""))),
    )
    saw_lower = False
    for row in ordered:
        tier = row.get("selected_tier")
        if tier in {"economy", "balanced"}:
            saw_lower = True
        if tier == "strong" and saw_lower:
            return True
    return False


def _parse_route(route: str) -> tuple[str, str, str]:
    if not route.startswith(ROUTE_PREFIX):
        return "unknown", "unknown", "unknown"
    parts = route.split("|")
    if len(parts) != 4:
        return "unknown", "unknown", "unknown"
    return parts[1], parts[2], parts[3]


def _screening_reason(
    *,
    adoptable: bool,
    screenable_tier: str | None,
    associated_passes: int,
    associated_failures: int,
    excluded_errors: int,
    recoveries: int,
    critical_violations: int,
) -> tuple[bool, str]:
    if adoptable:
        return False, "strict_quality_adoptable"
    if screenable_tier is None:
        return False, "tier_not_screenable"
    if associated_passes < 5:
        return False, "insufficient_passing_associations"
    if associated_failures:
        return False, "terminal_failures"
    if excluded_errors:
        return False, "route_non_task_errors"
    if recoveries:
        return False, "recovery_dependency"
    if critical_violations:
        return False, "critical_violations"
    if screenable_tier == "economy":
        return True, "economy_exposure_observational"
    return True, "balanced_normal_observational"


def build_experience_matrix(
    evidence_rows: Iterable[dict[str, object]],
) -> list[dict[str, object]]:
    grouped: dict[tuple[str, str], list[dict[str, object]]] = defaultdict(list)
    for row in evidence_rows:
        route = str(row.get("route_projection", ""))
        if route:
            grouped[(str(row.get("policy", "")), route)].append(row)

    matrix: list[dict[str, object]] = []
    for (policy, route), rows in sorted(grouped.items()):
        task_family, role, risk = _parse_route(route)
        eligible_rows = [
            row
            for row in rows
            if row.get("quality_credit_eligible") is True
            and row.get("terminal_verdict") in {"pass", "fail"}
        ]
        independent_tasks = {str(row["task_id"]) for row in eligible_rows}
        associated_tasks = {str(row["task_id"]) for row in rows}
        passed = len(
            {
                str(row["task_id"])
                for row in eligible_rows
                if row.get("terminal_verdict") == "pass"
            }
        )
        failed = len(
            {
                str(row["task_id"])
                for row in eligible_rows
                if row.get("terminal_verdict") == "fail"
            }
        )
        inconclusive = len(
            {
                str(row["task_id"])
                for row in rows
                if row not in eligible_rows
            }
        )
        conclusive = passed + failed
        pass_rate_ppm = passed * 1_000_000 // conclusive if conclusive else 0
        selected = Counter()
        static = Counter()
        quality_selected = set()
        for row in rows:
            selected.update(dict(row.get("selected_tiers", {})))
            static.update(dict(row.get("static_tiers", {})))
        for row in eligible_rows:
            quality_selected.update(dict(row.get("selected_tiers", {})))
        excluded_errors = sum(int(row.get("excluded_non_task_errors", 0)) for row in rows)
        ambiguities = sum(bool(row.get("attribution_ambiguity")) for row in rows)
        recoveries = sum(bool(row.get("recovery_dependency")) for row in rows)
        critical_violations = sum(int(row.get("critical_violations", 0)) for row in rows)
        strictly_eligible = bool(eligible_rows) and ambiguities == 0 and excluded_errors == 0
        adoptable = (
            quality_selected == {"economy"}
            and len(independent_tasks) >= 5
            and pass_rate_ppm >= 800_000
            and critical_violations == 0
            and recoveries == 0
            and ambiguities == 0
            and excluded_errors == 0
        )
        strict_experiment = (
            strictly_eligible
            and quality_selected == {"balanced"}
            and risk == "normal"
            and len(independent_tasks) >= 5
            and pass_rate_ppm >= 800_000
            and critical_violations == 0
            and recoveries == 0
        )
        associated_passes = len(
            {
                str(row["task_id"])
                for row in rows
                if row.get("associated_terminal_pass") is True
            }
        )
        associated_failures = len(
            {
                str(row["task_id"])
                for row in rows
                if row.get("associated_terminal_fail") is True
            }
        )
        selected_set = set(selected)
        screenable_tier = None
        if "economy" in selected_set:
            screenable_tier = "economy"
        elif selected_set == {"balanced"} and risk == "normal":
            screenable_tier = "balanced"
        controlled, screening_reason = _screening_reason(
            adoptable=adoptable,
            screenable_tier=screenable_tier,
            associated_passes=associated_passes,
            associated_failures=associated_failures,
            excluded_errors=excluded_errors,
            recoveries=recoveries,
            critical_violations=critical_violations,
        )
        if adoptable:
            grade = "adoptable"
        elif strict_experiment:
            grade = "experiment"
        elif conclusive:
            grade = "insufficient"
        else:
            grade = "inconclusive"
        latency_samples = [int(row.get("latency_ms", 0)) for row in rows]
        rule_ids = sorted(
            {
                str(rule_id)
                for row in rows
                for rule_id in list(row.get("excluded_error_rule_ids", []))
            }
        )
        matrix.append(
            {
                "policy": policy,
                "route_projection": route,
                "task_family": task_family,
                "role": role,
                "risk": risk,
                "independent_tasks": len(independent_tasks),
                "independent_episodes": len(independent_tasks),
                "associated_tasks": len(associated_tasks),
                "pass": passed,
                "fail": failed,
                "inconclusive": inconclusive,
                "pass_rate_ppm": pass_rate_ppm,
                "selected_tier_distribution": dict(sorted(selected.items())),
                "static_tier_distribution": dict(sorted(static.items())),
                "cost_micro_usd": sum(int(row.get("cost_micro_usd", 0)) for row in rows),
                "latency_ms_mean": (
                    sum(latency_samples) // len(latency_samples) if latency_samples else 0
                ),
                "guard_promotions": sum(int(row.get("guard_promotions", 0)) for row in rows),
                "excluded_non_task_errors": excluded_errors,
                "excluded_error_rule_ids": rule_ids,
                "attribution_ambiguities": ambiguities,
                "recovery_dependencies": recoveries,
                "critical_violations": critical_violations,
                "evidence_grade": grade,
                "quality_credit_eligible": strictly_eligible,
                "active_recommendation": "economy" if adoptable else "retain",
                "economy_experiment_candidate": controlled and screenable_tier == "balanced",
                "controlled_validation_candidate": controlled,
                "screening_reason": screening_reason,
            }
        )
    return matrix


def _load_jsonl(path: Path) -> list[dict[str, object]]:
    rows = []
    for line in path.read_text().splitlines():
        if line.strip():
            value = json.loads(line)
            if not isinstance(value, dict):
                raise ValueError(f"{path} contains a non-object row")
            rows.append(value)
    return rows


def _result_paths(run_dir: Path) -> list[Path]:
    paths = sorted(run_dir.glob("jobs/*/*/result.json"))
    if paths:
        return paths
    return sorted(run_dir.glob("*/result.json"))


def _load_trial_histories(run_dir: Path) -> dict[str, list[dict[str, object]]]:
    histories = {}
    for result_path in _result_paths(run_dir):
        raw = json.loads(result_path.read_text())
        if not isinstance(raw, dict):
            continue
        trajectory_path = result_path.parent / "agent" / "trajectory.json"
        if not trajectory_path.exists():
            continue
        trajectory = json.loads(trajectory_path.read_text())
        steps = trajectory.get("steps") if isinstance(trajectory, dict) else None
        if not isinstance(steps, list):
            continue
        history = []
        for step in steps:
            if not isinstance(step, dict) or "message" not in step:
                continue
            source = step.get("source")
            if source == "user":
                role = "user"
            elif source == "agent":
                role = "assistant"
            else:
                continue
            history.append({"role": role, "content": step["message"]})
        if history:
            histories[_task_identity(raw)] = history
    return histories


def _task_evidence_rows(
    raw: dict[str, object],
    decisions: list[dict[str, object]],
    summary: dict[str, object],
    outcome: Outcome,
) -> list[dict[str, object]]:
    by_route: dict[tuple[str, str], list[dict[str, object]]] = defaultdict(list)
    for row in decisions:
        by_route[(str(row.get("policy", "")), str(row.get("route_projection", "")))].append(row)
    rows = []
    for (policy, route), route_decisions in sorted(by_route.items()):
        selected = Counter(str(row.get("selected_tier", "unknown")) for row in route_decisions)
        static = Counter(str(row.get("static_tier", "unknown")) for row in route_decisions)
        errors = [
            classify_request_error(value)
            for value in (row.get("request_error") for row in route_decisions)
            if isinstance(value, dict)
        ]
        errors = [error for error in errors if error.category in NON_TASK_CATEGORIES]
        error_count = len(errors)
        error_rule_ids = {error.rule_id for error in errors}
        if error_count == 0 and int(summary.get("excluded_non_task_errors", 0)):
            # A terminal non-task exception has no finer request-to-route
            # identity. Conservatively contaminate every exercised cell rather
            # than allowing any of them to pass a direct-adoption gate.
            error_count = int(summary["excluded_non_task_errors"])
            error_rule_ids.update(
                str(rule_id)
                for rule_id in list(summary.get("excluded_error_rule_ids", []))
            )
        rows.append(
            {
                "task_id": _task_identity(raw),
                "policy": policy,
                "route_projection": route,
                "terminal_verdict": (
                    outcome.terminal_verdict
                    if summary.get("quality_credit_eligible") is True
                    else "inconclusive"
                ),
                "quality_credit_eligible": (
                    summary.get("quality_credit_eligible") is True and len(by_route) == 1
                ),
                "selected_tiers": dict(sorted(selected.items())),
                "static_tiers": dict(sorted(static.items())),
                "cost_micro_usd": sum(
                    int(row.get("cost_micro_usd", 0) or 0) for row in route_decisions
                ),
                "latency_ms": sum(int(row.get("latency_ms", 0) or 0) for row in route_decisions),
                "guard_promotions": sum(
                    bool(row.get("progress_clause_ids"))
                    or row.get("selected_tier") != row.get("static_tier")
                    for row in route_decisions
                ),
                "excluded_non_task_errors": error_count,
                "excluded_error_rule_ids": sorted(error_rule_ids),
                "attribution_ambiguity": summary.get("attribution_reason")
                in {"multiple_policies", "multiple_route_cells", "multiple_selected_tiers"},
                "recovery_dependency": _has_recovery_dependency(route_decisions),
                "critical_violations": 0,
                "associated_terminal_pass": outcome.terminal_verdict == "pass",
                "associated_terminal_fail": outcome.terminal_verdict == "fail",
                "attribution_reason": summary.get("attribution_reason"),
            }
        )
    return rows


def _write_jsonl(path: Path, rows: Iterable[dict[str, object]]) -> None:
    payload = b"".join(_json_bytes(row, sort_keys=True) + b"\n" for row in rows)
    path.write_bytes(payload)


def _write_matrix_csv(path: Path, rows: list[dict[str, object]]) -> None:
    with path.open("w", newline="", encoding="utf-8") as output:
        writer = csv.DictWriter(output, fieldnames=MATRIX_COLUMNS)
        writer.writeheader()
        for row in rows:
            rendered = {
                key: (
                    json.dumps(value, sort_keys=True, separators=(",", ":"))
                    if isinstance(value, (dict, list))
                    else value
                )
                for key, value in row.items()
            }
            writer.writerow(rendered)


def run(
    run_dir: Path,
    decisions_path: Path,
    output_dir: Path,
    traces_path: Path | None = None,
) -> None:
    decisions = _load_jsonl(decisions_path)
    if all(_valid_exact_decision(row) for row in decisions):
        join_summary = {"prejoined": len(decisions), "joined": 0, "ambiguous": 0, "unmatched": 0}
    else:
        if traces_path is None:
            raise ValueError(
                "decision rows lack exact task/ingress identity; provide --traces for exact content joining"
            )
        decisions, join_summary = join_raw_decisions(
            _load_trial_histories(run_dir), _load_jsonl(traces_path), decisions
        )
        join_summary["prejoined"] = 0
    by_task: dict[str, list[dict[str, object]]] = defaultdict(list)
    for row in decisions:
        task_id = row.get("exact_task_id")
        if isinstance(task_id, str) and task_id:
            by_task[task_id].append(row)

    packets = []
    evidence_rows: list[dict[str, object]] = []
    for result_path in _result_paths(run_dir):
        raw = json.loads(result_path.read_text())
        if not isinstance(raw, dict):
            raise ValueError(f"{result_path} is not a JSON object")
        task_id = _task_identity(raw)
        task_decisions = sorted(
            by_task.get(task_id, []),
            key=lambda row: (str(row.get("captured_at", "")), str(row.get("decision_id", ""))),
        )
        request_errors = [
            classify_request_error(value)
            for value in (row.get("request_error") for row in task_decisions)
            if isinstance(value, dict)
        ]
        request_errors = [
            error for error in request_errors if error.category in NON_TASK_CATEGORIES
        ]
        packet, summary = build_packet(raw, task_decisions, request_errors)
        if task_decisions and packet["subject"]["policy_digest"]:
            packets.append(packet)
        evidence_rows.extend(
            _task_evidence_rows(raw, task_decisions, summary, classify_outcome(raw))
        )

    matrix = build_experience_matrix(evidence_rows)
    output_dir.mkdir(parents=True, exist_ok=True)
    _write_jsonl(output_dir / "packets.jsonl", packets)
    _write_jsonl(output_dir / "task-evidence.jsonl", evidence_rows)
    (output_dir / "matrix.json").write_text(
        json.dumps(matrix, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    _write_matrix_csv(output_dir / "matrix.csv", matrix)
    (output_dir / "join-summary.json").write_text(
        json.dumps(join_summary, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Build conservative route evidence from exact-joined Harbor results"
    )
    parser.add_argument("--run-dir", type=Path, required=True)
    parser.add_argument("--decisions", type=Path, required=True)
    parser.add_argument(
        "--traces",
        type=Path,
        help="Raw trace JSONL; required when decision rows are not already exact-joined",
    )
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    run(args.run_dir, args.decisions, args.output_dir, args.traces)


if __name__ == "__main__":
    main()
