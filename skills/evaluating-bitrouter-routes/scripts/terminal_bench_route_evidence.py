#!/usr/bin/env python3
"""Build fail-closed Eval Exchange packets from Harbor/Terminal-Bench artifacts.

All benchmark parsing, request-error taxonomy, attribution, and observational
screening live in this external adapter. BitRouter receives only generic Eval
Exchange subjects and results.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import re
from collections import Counter, defaultdict
from datetime import datetime
from decimal import Decimal, InvalidOperation, ROUND_HALF_UP
from pathlib import Path
from typing import Iterable, NamedTuple


ADAPTER_VERSION = "terminal-bench-route-evidence-v2"
TAXONOMY_VERSION = "request-error-taxonomy-v1"
QUALITY_METRIC = "quality.pass"
ROUTE_PREFIX = "agent_route/v1|"
NON_TASK_CATEGORIES = {"provider", "network", "auth", "rate_limit", "transport"}
MAX_JSON_BYTES = 64 * 1024 * 1024
MAX_JSONL_LINE_BYTES = 16 * 1024 * 1024
MAX_JSONL_FILE_BYTES = 4 * 1024 * 1024 * 1024
MAX_JSONL_ROWS = 2_000_000
FORMULA_SIGILS = ("=", "+", "-", "@", "\t", "\r")


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
            for marker in (
                "upstream_unavailable",
                "upstream unavailable",
                "provider unavailable",
            )
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
    "coverage_failures",
    "recovery_dependencies",
    "critical_violations",
    "critical_violations_known",
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
        allow_nan=False,
    ).encode("utf-8")


def _digest(value: bytes | object) -> str:
    raw = value if isinstance(value, bytes) else _json_bytes(value, sort_keys=True)
    return "sha256:" + hashlib.sha256(raw).hexdigest()


def _required_string(raw: dict[str, object], field: str, context: str) -> str:
    value = raw.get(field)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{context} missing required {field}")
    return value


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


def classify_request_error(raw: dict[str, object]) -> RequestError:
    status_raw = raw.get("status", raw.get("http_status"))
    status = (
        status_raw
        if isinstance(status_raw, int) and not isinstance(status_raw, bool)
        else None
    )
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
        if not math.isfinite(reward):
            raise ValueError("terminal verifier reward must be finite")
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


def _task_identity(raw: dict[str, object]) -> str:
    exact = raw.get("exact_task_id")
    if isinstance(exact, str) and exact:
        return exact
    checksum = raw.get("task_checksum")
    if isinstance(checksum, str) and checksum:
        return checksum
    task_name = raw.get("task_name")
    if isinstance(task_name, str) and task_name:
        return task_name
    raise ValueError("trial result missing canonical task identity")


def _critical_violations(raw: dict[str, object]) -> int | None:
    value = raw.get("critical_violations")
    if value is None:
        artifact = raw.get("eval_artifact")
        value = artifact.get("critical_violations") if isinstance(artifact, dict) else None
    if value is None:
        return None
    if isinstance(value, list):
        return len(value)
    if not isinstance(value, int) or isinstance(value, bool) or value < 0:
        raise ValueError("critical_violations must be a non-negative integer or list")
    return value


def _decision_id(raw: dict[str, object]) -> str | None:
    for field in ("decision_id", "request_id"):
        value = raw.get(field)
        if isinstance(value, str) and value:
            return value
    return None


def _normalize_decisions(decisions: list[dict[str, object]]) -> list[dict[str, object]]:
    normalized = []
    ids: set[str] = set()
    commitments: set[str] = set()
    for index, original in enumerate(decisions, 1):
        row = dict(original)
        decision_id = _decision_id(row)
        if decision_id is None:
            raise ValueError(f"decision row {index} missing required decision identity")
        if decision_id in ids:
            raise ValueError(f"duplicate decision id {decision_id}")
        ids.add(decision_id)
        row["decision_id"] = decision_id
        commitment = row.get("ingress_request_id_sha256")
        if not isinstance(commitment, str) or not commitment:
            raise ValueError(
                f"decision {decision_id} missing required ingress_request_id_sha256"
            )
        if commitment in commitments:
            raise ValueError(f"duplicate decision ingress commitment {commitment}")
        commitments.add(commitment)
        normalized.append(row)
    return normalized


def _valid_exact_decision(raw: dict[str, object]) -> bool:
    return (
        raw.get("exact_ingress_join") is True
        and raw.get("task_join_method")
        in {"full_messages_prefix", "task_description_field"}
        and all(
            isinstance(raw.get(field), str) and bool(raw.get(field))
            for field in (
                "decision_id",
                "exact_task_id",
                "exact_trial_id",
                "physical_request_id",
                "policy",
                "policy_digest",
                "route_projection",
                "request_key",
                "selected_tier",
                "captured_at",
            )
        )
    )


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


def _attribution(
    decisions: list[dict[str, object]],
    outcome: Outcome,
    errors: list[RequestError],
    coverage_reasons: Iterable[str] = (),
) -> tuple[bool, str, str | None]:
    if list(coverage_reasons):
        return False, "request_coverage_incomplete", None
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
    representative = min(
        decisions,
        key=lambda row: (str(row["captured_at"]), str(row["decision_id"])),
    )
    if errors:
        return False, "non_task_error_contamination", str(representative["decision_id"])
    return True, "unique_route_cell_and_tier", str(representative["decision_id"])


def _evidence_items(
    raw: dict[str, object],
    outcome: Outcome,
    attribution_reason: str,
    identity_material: dict[str, object],
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
                    "identity": identity_material,
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
    *,
    coverage_reasons: Iterable[str] = (),
    source_digest: str | None = None,
    run_identity: str | None = None,
    trial_identity: str | None = None,
) -> tuple[dict[str, object], dict[str, object]]:
    outcome = classify_outcome(raw)
    errors = [error for error in request_errors if error.category in NON_TASK_CATEGORIES]
    if not errors and outcome.excluded_error is not None:
        errors = [outcome.excluded_error]
    coverage = sorted(set(coverage_reasons))
    eligible, reason, representative = _attribution(
        decisions, outcome, errors, coverage
    )
    copied_decisions = [
        _decision_ref(row) for row in decisions if _valid_exact_decision(row)
    ]
    policy_digest = str(copied_decisions[0]["policy_digest"]) if copied_decisions else ""
    task_id = _task_identity(raw)
    trial_name = trial_identity or str(raw.get("trial_name") or "unidentified-trial")
    result_id = str(raw.get("id") or "unidentified-result")
    source = source_digest or _digest({"result": raw, "decisions": decisions})
    attribution_digest = _digest(
        {
            "decisions": decisions,
            "request_errors": [error._asdict() for error in errors],
            "coverage_reasons": coverage,
            "outcome": outcome._asdict(),
        }
    )
    identity_material = {
        "adapter_version": ADAPTER_VERSION,
        "taxonomy_version": TAXONOMY_VERSION,
        "run_identity": run_identity or "direct-build",
        "source_digest": source,
        "canonical_task_id": task_id,
        "trial_identity": trial_name,
        "result_identity": result_id,
        "attribution_digest": attribution_digest,
    }
    identity_digest = _digest(identity_material).split(":", 1)[1]
    eval_id = "tb-route-" + identity_digest[:40]
    subject_id = "tb-subject-" + identity_digest[40:] + identity_digest[:8]
    evidence = _evidence_items(raw, outcome, reason, identity_material)
    evidence_digest = _digest(_json_bytes(evidence))
    if coverage:
        packet_verdict = "inconclusive"
    elif eligible or (errors and outcome.terminal_verdict in {"pass", "fail"}):
        packet_verdict = outcome.terminal_verdict
    else:
        packet_verdict = "inconclusive"
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
        "subject_id": subject_id,
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
            "version": ADAPTER_VERSION,
            "config_digest": config_digest,
        },
        "verdict": packet_verdict,
        "metrics": metrics,
        "hard_violations": [],
        "confidence_ppm": 1_000_000 if outcome.reward is not None else None,
        "evidence_refs": ["classification", "verifier"],
        "decision_credit": decision_credit,
        "idempotency_key": "tb-route-result-" + identity_digest,
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
    critical = _critical_violations(raw)
    summary: dict[str, object] = {
        "task_id": task_id,
        "trial_identity": trial_name,
        "policy": str(decisions[0].get("policy", "")) if decisions else "",
        "route_projection": route_projections[0] if len(route_projections) == 1 else "",
        "terminal_verdict": outcome.terminal_verdict,
        "quality_credit_eligible": eligible,
        "representative_decision_id": representative,
        "attribution_reason": reason,
        "coverage_reasons": coverage,
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
        in {
            "multiple_policies",
            "multiple_route_cells",
            "multiple_selected_tiers",
        },
        "recovery_dependency": _has_recovery_dependency(decisions),
        "critical_violations": critical,
        "critical_violations_known": critical is not None,
        "associated_terminal_pass": outcome.terminal_verdict == "pass",
        "associated_terminal_fail": outcome.terminal_verdict == "fail",
    }
    return {"subject": subject, "result": result}, summary


def _has_recovery_dependency(decisions: list[dict[str, object]]) -> bool:
    ordered = sorted(
        decisions,
        key=lambda row: (
            str(row.get("captured_at", "")),
            str(row.get("decision_id", "")),
        ),
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
    critical_known: bool,
    coverage_failures: int,
) -> tuple[bool, str]:
    if adoptable:
        return False, "strict_quality_adoptable"
    if coverage_failures:
        return False, "request_coverage_incomplete"
    if screenable_tier is None:
        return False, "tier_not_screenable"
    if not critical_known:
        return False, "critical_evidence_unknown"
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
        verdicts_by_task: dict[str, set[str]] = defaultdict(set)
        episodes = set()
        for row in eligible_rows:
            task = str(row["task_id"])
            verdicts_by_task[task].add(str(row["terminal_verdict"]))
            episodes.add(str(row.get("trial_identity", task)))
        passed_tasks = {
            task for task, verdicts in verdicts_by_task.items() if verdicts == {"pass"}
        }
        failed_tasks = {
            task for task, verdicts in verdicts_by_task.items() if "fail" in verdicts
        }
        independent_tasks = set(verdicts_by_task)
        associated_tasks = {str(row["task_id"]) for row in rows}
        passed = len(passed_tasks)
        failed = len(failed_tasks)
        inconclusive = len(associated_tasks - passed_tasks - failed_tasks)
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
        excluded_errors = sum(
            int(row.get("excluded_non_task_errors", 0)) for row in rows
        )
        ambiguities = sum(bool(row.get("attribution_ambiguity")) for row in rows)
        coverage_failures = sum(int(row.get("coverage_failures", 0)) for row in rows)
        recoveries = sum(bool(row.get("recovery_dependency")) for row in rows)
        critical_known = all(
            row.get("critical_violations_known") is True for row in rows
        )
        critical_violations = sum(
            int(row.get("critical_violations") or 0) for row in rows
        )
        strictly_eligible = (
            bool(eligible_rows)
            and ambiguities == 0
            and coverage_failures == 0
            and excluded_errors == 0
        )
        adoptable = (
            strictly_eligible
            and critical_known
            and quality_selected == {"economy"}
            and len(independent_tasks) >= 5
            and pass_rate_ppm >= 800_000
            and critical_violations == 0
            and recoveries == 0
        )
        strict_experiment = (
            strictly_eligible
            and critical_known
            and quality_selected == {"balanced"}
            and risk == "normal"
            and len(independent_tasks) >= 5
            and pass_rate_ppm >= 800_000
            and critical_violations == 0
            and recoveries == 0
        )
        associated_verdicts: dict[str, set[str]] = defaultdict(set)
        for row in rows:
            task = str(row["task_id"])
            if row.get("associated_terminal_pass") is True:
                associated_verdicts[task].add("pass")
            if row.get("associated_terminal_fail") is True:
                associated_verdicts[task].add("fail")
        associated_passes = sum(
            verdicts == {"pass"} for verdicts in associated_verdicts.values()
        )
        associated_failures = sum(
            "fail" in verdicts for verdicts in associated_verdicts.values()
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
            critical_known=critical_known,
            coverage_failures=coverage_failures,
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
                "independent_episodes": len(episodes),
                "associated_tasks": len(associated_tasks),
                "pass": passed,
                "fail": failed,
                "inconclusive": inconclusive,
                "pass_rate_ppm": pass_rate_ppm,
                "selected_tier_distribution": dict(sorted(selected.items())),
                "static_tier_distribution": dict(sorted(static.items())),
                "cost_micro_usd": sum(
                    int(row.get("cost_micro_usd", 0)) for row in rows
                ),
                "latency_ms_mean": (
                    sum(latency_samples) // len(latency_samples)
                    if latency_samples
                    else 0
                ),
                "guard_promotions": sum(
                    int(row.get("guard_promotions", 0)) for row in rows
                ),
                "excluded_non_task_errors": excluded_errors,
                "excluded_error_rule_ids": rule_ids,
                "attribution_ambiguities": ambiguities,
                "coverage_failures": coverage_failures,
                "recovery_dependencies": recoveries,
                "critical_violations": critical_violations if critical_known else None,
                "critical_violations_known": critical_known,
                "evidence_grade": grade,
                "quality_credit_eligible": strictly_eligible,
                "active_recommendation": "economy" if adoptable else "retain",
                "economy_experiment_candidate": (
                    controlled and screenable_tier == "balanced"
                ),
                "controlled_validation_candidate": controlled,
                "screening_reason": screening_reason,
            }
        )
    return matrix


def _load_jsonl(path: Path) -> list[dict[str, object]]:
    if path.stat().st_size > MAX_JSONL_FILE_BYTES:
        raise ValueError(f"{path} exceeds JSONL file byte bound")
    rows = []
    row_digests: set[str] = set()
    with path.open("rb") as source:
        line_number = 0
        while True:
            raw_line = source.readline(MAX_JSONL_LINE_BYTES + 1)
            if not raw_line:
                break
            line_number += 1
            if len(raw_line) > MAX_JSONL_LINE_BYTES:
                raise ValueError(f"{path} line {line_number} exceeds byte bound")
            if not raw_line.strip():
                continue
            try:
                value = json.loads(raw_line)
            except (UnicodeDecodeError, json.JSONDecodeError) as error:
                raise ValueError(f"{path} line {line_number} is invalid JSON: {error}") from error
            if not isinstance(value, dict):
                raise ValueError(f"{path} line {line_number} contains a non-object row")
            digest = _digest(value)
            if digest in row_digests:
                raise ValueError(f"{path} contains duplicate input row at line {line_number}")
            row_digests.add(digest)
            rows.append(value)
            if len(rows) > MAX_JSONL_ROWS:
                raise ValueError(f"{path} exceeds JSONL row bound")
    return rows


def _load_json_object(path: Path) -> dict[str, object]:
    if path.stat().st_size > MAX_JSON_BYTES:
        raise ValueError(f"{path} exceeds JSON byte bound")
    with path.open("rb") as source:
        raw = source.read(MAX_JSON_BYTES + 1)
    if len(raw) > MAX_JSON_BYTES:
        raise ValueError(f"{path} exceeds JSON byte bound")
    try:
        value = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError(f"{path} is invalid JSON: {error}") from error
    if not isinstance(value, dict):
        raise ValueError(f"{path} is not a JSON object")
    return value


def _file_digest(path: Path) -> str:
    digest = hashlib.sha256()
    total = 0
    with path.open("rb") as source:
        while True:
            chunk = source.read(1024 * 1024)
            if not chunk:
                break
            total += len(chunk)
            if total > MAX_JSONL_FILE_BYTES:
                raise ValueError(f"{path} exceeds source digest byte bound")
            digest.update(chunk)
    return "sha256:" + digest.hexdigest()


def _result_paths(run_dir: Path) -> list[Path]:
    return sorted(run_dir.rglob("result.json"))


def _trajectory_history(path: Path) -> list[dict[str, object]]:
    trajectory = _load_json_object(path)
    steps = trajectory.get("steps")
    if not isinstance(steps, list):
        raise ValueError(f"{path} missing steps")
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
    return history


def _load_trials(run_dir: Path) -> list[dict[str, object]]:
    trials = []
    trial_names: set[str] = set()
    result_ids: set[str] = set()
    row_digests: set[str] = set()
    for result_path in _result_paths(run_dir):
        raw = _load_json_object(result_path)
        if "trial_name" not in raw and (
            "n_total_trials" in raw or "stats" in raw
        ):
            continue
        trial_name = _required_string(raw, "trial_name", str(result_path))
        result_id = _required_string(raw, "id", str(result_path))
        canonical_task_id = _task_identity(raw)
        result_digest = _digest(raw)
        if result_digest in row_digests:
            raise ValueError(f"duplicate result row {result_path}")
        if trial_name in trial_names:
            raise ValueError(f"duplicate trial identity {trial_name}")
        if result_id in result_ids:
            raise ValueError(f"duplicate result identity {result_id}")
        trial_names.add(trial_name)
        result_ids.add(result_id)
        row_digests.add(result_digest)
        trajectory_path = result_path.parent / "agent" / "trajectory.json"
        history = _trajectory_history(trajectory_path) if trajectory_path.exists() else []
        trial_identity = f"{trial_name}:{result_id}"
        trials.append(
            {
                "raw": raw,
                "canonical_task_id": canonical_task_id,
                "trial_name": trial_name,
                "trial_identity": trial_identity,
                "result_id": result_id,
                "history": history,
                "result_path": result_path,
                "trajectory_path": trajectory_path if trajectory_path.exists() else None,
            }
        )
    if not trials:
        raise ValueError(f"{run_dir} contains no trial results")
    return trials


def _request_cost_micro_usd(raw: dict[str, object]) -> int:
    direct = raw.get("cost_micro_usd")
    if direct is not None:
        if not isinstance(direct, int) or isinstance(direct, bool) or direct < 0:
            raise ValueError("cost_micro_usd must be a non-negative integer")
        return direct
    nominal = raw.get("nominal_cost_usd")
    if nominal is None:
        return 0
    try:
        value = Decimal(str(nominal))
    except InvalidOperation as error:
        raise ValueError("nominal_cost_usd must be a finite decimal") from error
    if not value.is_finite() or value < 0:
        raise ValueError("nominal_cost_usd must be a finite non-negative decimal")
    return int((value * Decimal(1_000_000)).quantize(Decimal("1"), rounding=ROUND_HALF_UP))


def _parse_time(value: object) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    normalized = re.sub(r"(\.\d{6})\d+(?=Z|[+-]\d{2}:\d{2}$)", r"\1", value)
    normalized = normalized[:-1] + "+00:00" if normalized.endswith("Z") else normalized
    try:
        return datetime.fromisoformat(normalized)
    except ValueError as error:
        raise ValueError(f"invalid request accounting timestamp {value}") from error


def _request_latency_ms(raw: dict[str, object]) -> int:
    direct = raw.get("latency_ms")
    if direct is not None:
        if not isinstance(direct, int) or isinstance(direct, bool) or direct < 0:
            raise ValueError("latency_ms must be a non-negative integer")
        return direct
    start = _parse_time(raw.get("decision_at"))
    finish = _parse_time(raw.get("created_at"))
    if start is None or finish is None:
        return 0
    latency = int((finish - start).total_seconds() * 1000)
    if latency < 0:
        raise ValueError("request outcome latency timestamps are reversed")
    return latency


def _normalize_request_outcomes(
    outcomes: list[dict[str, object]],
) -> dict[str, dict[str, object]]:
    indexed = {}
    for index, original in enumerate(outcomes, 1):
        request_id = _required_string(
            original, "request_id", f"request outcome row {index}"
        )
        if request_id in indexed:
            raise ValueError(f"duplicate request outcome id {request_id}")
        if "error" not in original:
            raise ValueError(f"request outcome {request_id} missing explicit error field")
        row = dict(original)
        row["cost_micro_usd"] = _request_cost_micro_usd(row)
        row["latency_ms"] = _request_latency_ms(row)
        error = row.get("error")
        if error is None:
            row["classified_error"] = None
        elif isinstance(error, dict):
            row["classified_error"] = classify_request_error(error)
        else:
            row["classified_error"] = classify_request_error({"error": error})
        for token_field in (
            "prompt_tokens",
            "completion_tokens",
            "reasoning_tokens",
            "cache_read_tokens",
            "cache_write_tokens",
            "uncached_input_tokens",
            "visible_output_tokens",
        ):
            value = row.get(token_field)
            if value is not None and (
                not isinstance(value, int) or isinstance(value, bool) or value < 0
            ):
                raise ValueError(
                    f"request outcome {request_id} has invalid {token_field}"
                )
        indexed[request_id] = row
    return indexed


def _trial_match_indexes(
    trials: list[dict[str, object]],
) -> tuple[dict[str, set[str]], dict[str, set[str]]]:
    prefix_to_trials: dict[str, set[str]] = defaultdict(set)
    description_to_trials: dict[str, set[str]] = defaultdict(set)
    for trial in trials:
        trial_identity = str(trial["trial_identity"])
        prefix = []
        for message in list(trial["history"]):
            prefix.append(message)
            if message.get("role") == "user":
                prefix_to_trials[_message_hash(prefix)].add(trial_identity)
                description = _task_description_hash(message.get("content"))
                if description is not None:
                    description_to_trials[description].add(trial_identity)
    return prefix_to_trials, description_to_trials


def _trace_task_candidates(
    trace: dict[str, object],
    prefix_to_trials: dict[str, set[str]],
    description_to_trials: dict[str, set[str]],
) -> tuple[set[str], str | None]:
    raw_body = trace.get("raw_body")
    raw_messages = raw_body.get("messages") if isinstance(raw_body, dict) else None
    if not isinstance(raw_messages, list) or not all(
        isinstance(message, dict) for message in raw_messages
    ):
        return set(), None
    messages = list(raw_messages)
    candidates = set(prefix_to_trials.get(_message_hash(messages), set()))
    if candidates:
        return candidates, "full_messages_prefix"
    descriptions = {
        description
        for description in (
            _task_description_hash(message.get("content"))
            for message in messages
            if message.get("role") == "user"
        )
        if description is not None
    }
    described_trials = set()
    for description in descriptions:
        described_trials.update(description_to_trials.get(description, set()))
    if described_trials:
        return described_trials, "task_description_field"
    return set(), None


def join_raw_decisions(
    trial_histories: dict[str, list[dict[str, object]]],
    traces: list[dict[str, object]],
    decisions: list[dict[str, object]],
) -> tuple[list[dict[str, object]], dict[str, int]]:
    """Compatibility helper for exact trace/task/decision join unit tests."""
    trials = [
        {
            "trial_identity": task_id,
            "canonical_task_id": task_id,
            "history": history,
        }
        for task_id, history in trial_histories.items()
    ]
    normalized = _normalize_decisions(decisions)
    prefix_to_trials, description_to_trials = _trial_match_indexes(trials)
    decisions_by_ingress = {
        str(row["ingress_request_id_sha256"]): row for row in normalized
    }
    joined = []
    summary = {"joined": 0, "ambiguous": 0, "unmatched": 0}
    for trace in traces:
        candidates, method = _trace_task_candidates(
            trace, prefix_to_trials, description_to_trials
        )
        trace_id = trace.get("id")
        decision_row = (
            decisions_by_ingress.get(ingress_commitment(str(trace_id)))
            if isinstance(trace_id, str) and trace_id
            else None
        )
        if len(candidates) == 1 and decision_row is not None and method:
            trial_identity = next(iter(candidates))
            row = dict(decision_row)
            row["exact_task_id"] = trial_identity
            row["exact_trial_id"] = trial_identity
            row["physical_request_id"] = trace_id
            row["task_join_method"] = method
            row["exact_ingress_join"] = True
            joined.append(row)
            summary["joined"] += 1
        elif len(candidates) > 1:
            summary["ambiguous"] += 1
        else:
            summary["unmatched"] += 1
    return joined, summary


def _task_coverage_reasons(
    trials: list[dict[str, object]], trial_reasons: dict[str, set[str]]
) -> dict[str, list[str]]:
    result: dict[str, set[str]] = defaultdict(set)
    trial_by_id = {str(trial["trial_identity"]): trial for trial in trials}
    for trial_identity, reasons in trial_reasons.items():
        task_id = str(trial_by_id[trial_identity]["canonical_task_id"])
        result[task_id].update(reasons)
    return {key: sorted(value) for key, value in sorted(result.items())}


def _join_run_evidence(
    trials: list[dict[str, object]],
    traces: list[dict[str, object]],
    decisions: list[dict[str, object]],
    request_outcomes: list[dict[str, object]],
) -> tuple[list[dict[str, object]], dict[str, object], dict[str, set[str]]]:
    normalized_decisions = _normalize_decisions(decisions)
    outcomes_by_id = _normalize_request_outcomes(request_outcomes)
    trial_by_id = {str(trial["trial_identity"]): trial for trial in trials}
    prefix_to_trials, description_to_trials = _trial_match_indexes(trials)
    decisions_by_ingress = {
        str(row["ingress_request_id_sha256"]): row for row in normalized_decisions
    }
    trace_ids: set[str] = set()
    for index, trace in enumerate(traces, 1):
        trace_id = _required_string(trace, "id", f"trace row {index}")
        if trace_id in trace_ids:
            raise ValueError(f"duplicate trace id {trace_id}")
        trace_ids.add(trace_id)

    joined = []
    consumed_decisions: set[str] = set()
    consumed_outcomes: set[str] = set()
    trials_with_trace: set[str] = set()
    trial_reasons: dict[str, set[str]] = defaultdict(set)
    global_reasons: set[str] = set()
    summary: dict[str, object] = {
        "trace_count": len(traces),
        "decision_count": len(normalized_decisions),
        "request_outcome_count": len(request_outcomes),
        "joined": 0,
        "audit_joined": 0,
        "ambiguous": 0,
        "unmatched": 0,
    }
    for trace in traces:
        trace_id = str(trace["id"])
        candidates, method = _trace_task_candidates(
            trace, prefix_to_trials, description_to_trials
        )
        if len(candidates) != 1 or method is None:
            ambiguous = len(candidates) > 1
            reason = (
                f"trace_task_ambiguous:{trace_id}"
                if ambiguous
                else f"trace_task_unmatched:{trace_id}"
            )
            global_reasons.add(reason)
            key = "ambiguous" if ambiguous else "unmatched"
            summary[key] = int(summary[key]) + 1
            continue
        trial_identity = next(iter(candidates))
        trial = trial_by_id[trial_identity]
        trials_with_trace.add(trial_identity)
        decision_row = decisions_by_ingress.get(ingress_commitment(trace_id))
        outcome_row = outcomes_by_id.get(trace_id)
        if decision_row is not None:
            consumed_decisions.add(str(decision_row["decision_id"]))
        if outcome_row is not None:
            consumed_outcomes.add(trace_id)
        if decision_row is None:
            trial_reasons[trial_identity].add("trace_missing_decision")
        if outcome_row is None:
            trial_reasons[trial_identity].add("trace_missing_request_outcome")
        if decision_row is None:
            summary["unmatched"] = int(summary["unmatched"]) + 1
            continue
        row = dict(decision_row)
        row["exact_task_id"] = str(trial["canonical_task_id"])
        row["exact_trial_id"] = trial_identity
        row["physical_request_id"] = trace_id
        row["task_join_method"] = method
        row["exact_ingress_join"] = True
        row["cost_micro_usd"] = (
            int(outcome_row["cost_micro_usd"]) if outcome_row is not None else 0
        )
        row["latency_ms"] = (
            int(outcome_row["latency_ms"]) if outcome_row is not None else 0
        )
        row["request_error_classification"] = (
            outcome_row["classified_error"] if outcome_row is not None else None
        )
        if not _valid_exact_decision(row):
            trial_reasons[trial_identity].add("decision_missing_required_identity")
            summary["unmatched"] = int(summary["unmatched"]) + 1
            continue
        joined.append(row)
        if outcome_row is None:
            summary["unmatched"] = int(summary["unmatched"]) + 1
            summary["audit_joined"] = int(summary["audit_joined"]) + 1
        else:
            summary["joined"] = int(summary["joined"]) + 1

    for trial in trials:
        trial_identity = str(trial["trial_identity"])
        if not trial["history"]:
            trial_reasons[trial_identity].add("trial_trajectory_missing_or_empty")
        if trial_identity not in trials_with_trace:
            trial_reasons[trial_identity].add("trial_has_no_traces")
    for row in normalized_decisions:
        decision_id = str(row["decision_id"])
        if decision_id not in consumed_decisions:
            global_reasons.add(f"unconsumed_decision:{decision_id}")
    for request_id in outcomes_by_id:
        if request_id not in consumed_outcomes:
            global_reasons.add(f"unconsumed_request_outcome:{request_id}")

    classified_errors = [
        row["classified_error"]
        for row in outcomes_by_id.values()
        if isinstance(row.get("classified_error"), RequestError)
        and row["classified_error"].category in NON_TASK_CATEGORIES
    ]
    category_counts = Counter(error.category for error in classified_errors)
    rule_counts = Counter(error.rule_id for error in classified_errors)
    summary.update(
        {
            "consumed_decisions": len(consumed_decisions),
            "consumed_request_outcomes": len(consumed_outcomes),
            "global_quality_blocked": bool(global_reasons),
            "global_coverage_reasons": sorted(global_reasons),
            "trial_coverage_reasons": {
                key: sorted(value) for key, value in sorted(trial_reasons.items())
            },
            "task_coverage_reasons": _task_coverage_reasons(trials, trial_reasons),
            "excluded_non_task_errors": len(classified_errors),
            "excluded_error_categories": dict(sorted(category_counts.items())),
            "excluded_error_rule_ids": dict(sorted(rule_counts.items())),
        }
    )
    return joined, summary, trial_reasons


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
    task_recovery = _has_recovery_dependency(decisions)
    critical = summary.get("critical_violations")
    critical_known = summary.get("critical_violations_known") is True
    coverage_reasons = list(summary.get("coverage_reasons", []))
    for (policy, route), route_decisions in sorted(by_route.items()):
        selected = Counter(
            str(row.get("selected_tier", "unknown")) for row in route_decisions
        )
        static = Counter(
            str(row.get("static_tier", "unknown")) for row in route_decisions
        )
        errors = [
            row.get("request_error_classification") for row in route_decisions
        ]
        errors = [
            error
            for error in errors
            if isinstance(error, RequestError)
            and error.category in NON_TASK_CATEGORIES
        ]
        error_count = len(errors)
        error_rule_ids = {error.rule_id for error in errors}
        if error_count == 0 and outcome.excluded_error is not None:
            error_count = 1
            error_rule_ids.add(outcome.excluded_error.rule_id)
        rows.append(
            {
                "task_id": _task_identity(raw),
                "trial_identity": str(summary.get("trial_identity", "")),
                "policy": policy,
                "route_projection": route,
                "terminal_verdict": (
                    outcome.terminal_verdict
                    if summary.get("quality_credit_eligible") is True
                    and len(by_route) == 1
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
                "latency_ms": sum(
                    int(row.get("latency_ms", 0) or 0) for row in route_decisions
                ),
                "guard_promotions": sum(
                    bool(row.get("progress_clause_ids"))
                    or row.get("selected_tier") != row.get("static_tier")
                    for row in route_decisions
                ),
                "excluded_non_task_errors": error_count,
                "excluded_error_rule_ids": sorted(error_rule_ids),
                "attribution_ambiguity": summary.get("attribution_reason")
                in {
                    "multiple_policies",
                    "multiple_route_cells",
                    "multiple_selected_tiers",
                },
                "coverage_failures": len(coverage_reasons),
                "coverage_reasons": coverage_reasons,
                "recovery_dependency": task_recovery,
                "critical_violations": critical,
                "critical_violations_known": critical_known,
                "associated_terminal_pass": outcome.terminal_verdict == "pass",
                "associated_terminal_fail": outcome.terminal_verdict == "fail",
                "attribution_reason": summary.get("attribution_reason"),
            }
        )
    return rows


def _source_digest(
    run_dir: Path,
    trials: list[dict[str, object]],
    traces_path: Path,
    decisions_path: Path,
    request_outcomes_path: Path,
) -> str:
    trial_sources = []
    for trial in trials:
        result_path = Path(trial["result_path"])
        trajectory_path = trial.get("trajectory_path")
        trial_sources.append(
            {
                "trial_identity": trial["trial_identity"],
                "result_digest": _file_digest(result_path),
                "trajectory_digest": (
                    _file_digest(Path(trajectory_path)) if trajectory_path else None
                ),
            }
        )
    return _digest(
        {
            "adapter_version": ADAPTER_VERSION,
            "taxonomy_version": TAXONOMY_VERSION,
            "run_identity": run_dir.name,
            "traces": _file_digest(traces_path),
            "decisions": _file_digest(decisions_path),
            "request_outcomes": _file_digest(request_outcomes_path),
            "trials": sorted(trial_sources, key=lambda row: str(row["trial_identity"])),
        }
    )


def _write_jsonl(path: Path, rows: Iterable[dict[str, object]]) -> None:
    with path.open("wb") as output:
        for row in rows:
            output.write(_json_bytes(row, sort_keys=True))
            output.write(b"\n")


def _csv_safe(value: object) -> object:
    if isinstance(value, str) and value.startswith(FORMULA_SIGILS):
        return "'" + value
    return value


def _write_matrix_csv(path: Path, rows: list[dict[str, object]]) -> None:
    with path.open("w", newline="", encoding="utf-8") as output:
        writer = csv.DictWriter(output, fieldnames=MATRIX_COLUMNS)
        writer.writeheader()
        for row in rows:
            rendered = {
                key: _csv_safe(
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
    request_outcomes_path: Path | None = None,
) -> None:
    if traces_path is None:
        raise ValueError("--traces is required for complete request coverage")
    if request_outcomes_path is None:
        raise ValueError(
            "--request-outcomes is required for authoritative request coverage"
        )
    trials = _load_trials(run_dir)
    traces = _load_jsonl(traces_path)
    decisions = _load_jsonl(decisions_path)
    request_outcomes = _load_jsonl(request_outcomes_path)
    joined, join_summary, trial_reasons = _join_run_evidence(
        trials, traces, decisions, request_outcomes
    )
    source_digest = _source_digest(
        run_dir, trials, traces_path, decisions_path, request_outcomes_path
    )
    by_trial: dict[str, list[dict[str, object]]] = defaultdict(list)
    for row in joined:
        by_trial[str(row["exact_trial_id"])].append(row)

    global_reasons = list(join_summary["global_coverage_reasons"])
    packets = []
    evidence_rows: list[dict[str, object]] = []
    for trial in sorted(trials, key=lambda row: str(row["trial_identity"])):
        raw = dict(trial["raw"])
        trial_identity = str(trial["trial_identity"])
        task_decisions = sorted(
            by_trial.get(trial_identity, []),
            key=lambda row: (
                str(row.get("captured_at", "")),
                str(row.get("decision_id", "")),
            ),
        )
        coverage_reasons = sorted(
            set(global_reasons) | set(trial_reasons.get(trial_identity, set()))
        )
        request_errors = [
            row.get("request_error_classification") for row in task_decisions
        ]
        request_errors = [
            error
            for error in request_errors
            if isinstance(error, RequestError)
            and error.category in NON_TASK_CATEGORIES
        ]
        packet, summary = build_packet(
            raw,
            task_decisions,
            request_errors,
            coverage_reasons=coverage_reasons,
            source_digest=source_digest,
            run_identity=run_dir.name,
            trial_identity=trial_identity,
        )
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
        json.dumps(matrix, indent=2, sort_keys=True, allow_nan=False) + "\n",
        encoding="utf-8",
    )
    _write_matrix_csv(output_dir / "matrix.csv", matrix)
    (output_dir / "join-summary.json").write_text(
        json.dumps(join_summary, indent=2, sort_keys=True, allow_nan=False) + "\n",
        encoding="utf-8",
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
        required=True,
        help="Authoritative raw request trace JSONL",
    )
    parser.add_argument(
        "--request-outcomes",
        type=Path,
        required=True,
        help="Authoritative request outcome/cost JSONL keyed by physical request_id",
    )
    parser.add_argument("--output-dir", type=Path, required=True)
    args = parser.parse_args()
    run(
        args.run_dir,
        args.decisions,
        args.output_dir,
        args.traces,
        args.request_outcomes,
    )


if __name__ == "__main__":
    main()
