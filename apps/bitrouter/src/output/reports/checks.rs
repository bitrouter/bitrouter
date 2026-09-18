//! Human views for request-check inventory, probes, and process-local receipts.

use bitrouter_sdk::language_model::receipts::{
    RequestCheckDispatchStatus, RequestCheckStatus, RequestDeliveryStatus, RequestFailureStage,
    RequestReceipt, RequestReceiptList, RequestReceiptLookup, RequestReceiptOutcome,
    RequestReceiptStoreHealth, RequestReceiptUnknownReason,
};
use bitrouter_sdk::language_model::request_checks::{
    CheckerFailureKind, RequestCheckCoverageScope, RequestCheckCoverageStatus,
};

use crate::actions::checks::{CheckerProbeReport, ChecksReport};
use crate::output::CliReport;
use crate::output::human::{Human, Table};
use crate::reload::{RunningConfigState, SavedConfigState};
use crate::request_checks::{CheckerProbeDecision, ProbeProtocolStatus, ProbeReachability};

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn check_status(receipt: &RequestReceipt) -> String {
    if receipt.checks.is_empty() {
        return "pending".into();
    }
    receipt
        .checks
        .iter()
        .map(|check| match check.status {
            RequestCheckStatus::NotRun => "not_run",
            RequestCheckStatus::Pending => "pending",
            RequestCheckStatus::NotEnabled => "not_enabled",
            RequestCheckStatus::Allowed => "allowed",
            RequestCheckStatus::Denied => "denied",
            RequestCheckStatus::Failed => "failed",
            RequestCheckStatus::Skipped => "skipped",
            RequestCheckStatus::Interrupted => "interrupted",
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn outcome(outcome: Option<RequestReceiptOutcome>) -> &'static str {
    match outcome {
        None => "active",
        Some(RequestReceiptOutcome::Completed) => "completed",
        Some(RequestReceiptOutcome::Denied) => "denied",
        Some(RequestReceiptOutcome::Failed) => "failed",
        Some(RequestReceiptOutcome::ClientDisconnected) => "client_disconnected",
        Some(RequestReceiptOutcome::Cancelled) => "cancelled",
    }
}

fn delivery(status: RequestDeliveryStatus) -> &'static str {
    match status {
        RequestDeliveryStatus::NotStarted => "not_started",
        RequestDeliveryStatus::NotApplicable => "not_applicable",
        RequestDeliveryStatus::ServerCommitted => "server_committed",
        RequestDeliveryStatus::Disconnected => "disconnected",
        RequestDeliveryStatus::Failed => "failed",
        RequestDeliveryStatus::Unknown => "unknown",
    }
}

fn failure_stage(stage: RequestFailureStage) -> &'static str {
    match stage {
        RequestFailureStage::PreRequest => "pre_request",
        RequestFailureStage::RequestCheck => "request_check",
        RequestFailureStage::Route => "route",
        RequestFailureStage::Upstream => "upstream",
        RequestFailureStage::Delivery => "delivery",
        RequestFailureStage::Internal => "internal",
    }
}

fn checker_failure(kind: CheckerFailureKind) -> &'static str {
    match kind {
        CheckerFailureKind::InputTooLarge => "input_too_large",
        CheckerFailureKind::Timeout => "timeout",
        CheckerFailureKind::Unavailable => "unavailable",
        CheckerFailureKind::InvalidResponse => "invalid_response",
        CheckerFailureKind::NotConfigured => "not_configured",
        CheckerFailureKind::Internal => "internal",
    }
}

impl CliReport for RequestReceiptList {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.field("incarnation", &self.incarnation_id)?;
        human.field("capacity", self.capacity)?;
        human.field("completed ttl", format!("{}s", self.completed_ttl_secs))?;
        human.field(
            "health",
            match self.health {
                RequestReceiptStoreHealth::Healthy => "healthy",
                RequestReceiptStoreHealth::Unavailable => "unavailable",
            },
        )?;
        if self.health == RequestReceiptStoreHealth::Unavailable {
            return human.note(
                "The current process cannot list retained receipts authoritatively; restart the daemon before relying on this store.",
            );
        }
        if self.receipts.is_empty() {
            return human.line("(no retained receipts in this process)");
        }
        human.blank()?;
        let mut table = Table::new([
            "RECEIPT", "REQUEST", "ROUTER", "CHECK", "UPSTREAM", "OUTCOME", "DELIVERY",
        ]);
        for receipt in &self.receipts {
            table.push([
                receipt.identity.receipt_id.clone(),
                receipt.identity.request_id.clone(),
                receipt.identity.router_id.clone(),
                check_status(receipt),
                yes_no(receipt.upstream_started).into(),
                outcome(receipt.outcome).into(),
                delivery(receipt.delivery).into(),
            ]);
        }
        human.table(&table)
    }
}

impl CliReport for RequestReceiptLookup {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        match self {
            Self::Found {
                receipt,
                retained_matches,
            } => {
                human.line("request receipt")?;
                human.field("receipt", &receipt.identity.receipt_id)?;
                human.field("request", &receipt.identity.request_id)?;
                human.field(
                    "selection",
                    match retained_matches {
                        0 | 1 => "one retained attempt matched".to_string(),
                        count => format!("newest of {count} retained attempts"),
                    },
                )?;
                human.field("incarnation", &receipt.identity.incarnation_id)?;
                human.field("router", &receipt.identity.router_id)?;
                human.field("router binding", &receipt.identity.router_binding_digest)?;
                human.field("upstream started", yes_no(receipt.upstream_started))?;
                human.field("outcome", outcome(receipt.outcome))?;
                human.field("delivery", delivery(receipt.delivery))?;
                if let Some(stage) = receipt.failure_stage {
                    human.field("failure stage", failure_stage(stage))?;
                }
                if receipt.checks.is_empty() {
                    human.field("checks", "pending")?;
                } else {
                    for check in &receipt.checks {
                        human.field(
                            "check",
                            format!(
                                "{}: {}",
                                check.checker_id.as_deref().unwrap_or("not_enabled"),
                                match check.status {
                                    RequestCheckStatus::NotRun => "not_run",
                                    RequestCheckStatus::Pending => "pending",
                                    RequestCheckStatus::NotEnabled => "not_enabled",
                                    RequestCheckStatus::Allowed => "allowed",
                                    RequestCheckStatus::Denied => "denied",
                                    RequestCheckStatus::Failed => "failed",
                                    RequestCheckStatus::Skipped => "skipped",
                                    RequestCheckStatus::Interrupted => "interrupted",
                                }
                            ),
                        )?;
                        if let Some(binding) = &check.binding_digest {
                            human.field("check binding", binding)?;
                        }
                        if let Some(invocation) = &check.invocation_id {
                            human.field("check invocation", invocation)?;
                        }
                        if let Some(dispatch) = check.dispatch {
                            human.field(
                                "check dispatch",
                                match dispatch {
                                    RequestCheckDispatchStatus::NotAttempted => "not_attempted",
                                    RequestCheckDispatchStatus::Attempted => "attempted",
                                    RequestCheckDispatchStatus::ResponseReceived => {
                                        "response_received"
                                    }
                                },
                            )?;
                        }
                        if let Some(reason) = &check.reason_code {
                            human.field("check reason", reason)?;
                        }
                        if let Some(failure) = check.failure_kind {
                            human.field("check failure", checker_failure(failure))?;
                        }
                        if let Some(version) = &check.implementation_version {
                            human.field("check implementation", version)?;
                        }
                        if let Some(coverage) = &check.coverage {
                            let scope = match coverage.scope {
                                RequestCheckCoverageScope::EntryRequestText => "entry_request_text",
                            };
                            let status = match coverage.status {
                                RequestCheckCoverageStatus::CompleteWithinScope => {
                                    "complete_within_scope"
                                }
                                RequestCheckCoverageStatus::InputTooLarge => "input_too_large",
                            };
                            human.field(
                                "check coverage",
                                format!(
                                    "{scope}; {status}; {} bytes; {} fragments; {} media excluded",
                                    coverage.text_bytes,
                                    coverage.text_fragments,
                                    coverage.excluded_media_fragments,
                                ),
                            )?;
                        }
                    }
                }
                Ok(())
            }
            Self::Unknown {
                request_id,
                requested_incarnation,
                current_incarnation,
                reason,
            } => {
                human.line("request receipt unknown")?;
                human.field("lookup id", request_id)?;
                if let Some(incarnation) = requested_incarnation {
                    human.field("requested incarnation", incarnation)?;
                }
                human.field("current incarnation", current_incarnation)?;
                human.field(
                    "reason",
                    match reason {
                        RequestReceiptUnknownReason::IncarnationMismatch => "incarnation_mismatch",
                        RequestReceiptUnknownReason::NotRetained => "not_retained",
                    },
                )?;
                human.note(
                    "No matching current-process evidence is retained; this does not prove the request succeeded, failed, or never ran.",
                )
            }
            Self::Unavailable {
                request_id,
                current_incarnation,
            } => {
                human.line("request receipt unavailable")?;
                human.field("lookup id", request_id)?;
                human.field("current incarnation", current_incarnation)?;
                human.note("The current process cannot answer receipt queries authoritatively; restart the daemon before relying on this store.")
            }
        }
    }
}

// These implementations are completed beside the runtime types so human and
// JSON output stay on the same typed reports.
impl CliReport for ChecksReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.field("resolved via", &self.resolved_via)?;
        human.field("incarnation", &self.receipt_retention.incarnation_id)?;
        human.field(
            "receipt health",
            match self.receipt_retention.health {
                RequestReceiptStoreHealth::Healthy => "healthy",
                RequestReceiptStoreHealth::Unavailable => "unavailable",
            },
        )?;
        if let Some(state) = &self.config_state {
            human.field(
                "saved config",
                match state.saved {
                    SavedConfigState::Available => "available",
                    SavedConfigState::Generated => "generated",
                    SavedConfigState::Missing => "missing",
                    SavedConfigState::Invalid => "invalid",
                    SavedConfigState::Unavailable => "unavailable",
                },
            )?;
            human.field(
                "running config",
                match state.running {
                    RunningConfigState::InSync => "in_sync",
                    RunningConfigState::ReloadRequired => "reload_required",
                    RunningConfigState::RestartRequired => "restart_required",
                    RunningConfigState::Mixed => "mixed",
                    RunningConfigState::Unknown => "unknown",
                },
            )?;
            if !state.restart_required_fields.is_empty() {
                human.field("restart required", state.restart_required_fields.join(", "))?;
            }
        } else {
            human.field("config state", "unknown")?;
        }
        if self.checkers.is_empty() {
            return human.line("(no running request checkers)");
        }
        human.blank()?;
        let mut checkers = Table::new([
            "CHECKER",
            "EXECUTION",
            "ENDPOINT FINGERPRINT",
            "CREDENTIAL",
            "CONTRACT",
            "BINDINGS",
            "LAST PROBE",
        ]);
        for checker in &self.checkers {
            checkers.push([
                checker.checker_id.clone(),
                match checker.execution {
                    crate::request_checks::CheckerExecution::Http => "http",
                    crate::request_checks::CheckerExecution::Native => "native",
                }
                .to_owned(),
                checker
                    .endpoint_fingerprint
                    .clone()
                    .unwrap_or_else(|| "not_applicable".to_owned()),
                if checker.credential_ready {
                    checker.credential_env.as_deref().unwrap_or("not_required")
                } else {
                    "missing"
                }
                .to_string(),
                checker.contract_version.to_string(),
                checker.bindings.len().to_string(),
                checker
                    .last_probe
                    .as_ref()
                    .map(|probe| {
                        format!(
                            "{}/{} @{}",
                            match probe.reachability {
                                ProbeReachability::NotAttempted => "not_attempted",
                                ProbeReachability::Unknown => "unknown",
                                ProbeReachability::Reachable => "reachable",
                                ProbeReachability::Unreachable => "unreachable",
                            },
                            match probe.protocol {
                                ProbeProtocolStatus::NotChecked => "not_checked",
                                ProbeProtocolStatus::Incomplete => "incomplete",
                                ProbeProtocolStatus::Valid => "valid",
                                ProbeProtocolStatus::Invalid => "invalid",
                            },
                            probe.observed_at_unix_ms
                        )
                    })
                    .unwrap_or_else(|| "never".into()),
            ]);
        }
        human.table(&checkers)?;
        let bindings = self
            .checkers
            .iter()
            .flat_map(|checker| {
                checker
                    .bindings
                    .iter()
                    .map(move |binding| (checker.checker_id.as_str(), binding))
            })
            .collect::<Vec<_>>();
        if !bindings.is_empty() {
            human.blank()?;
            let mut table = Table::new([
                "CHECKER",
                "ROUTER",
                "BINDING",
                "TIMEOUT",
                "MAX INPUT",
                "LAST ACTUAL",
            ]);
            for (checker, binding) in bindings {
                table.push([
                    checker.to_string(),
                    binding.router_id.clone(),
                    binding.binding_digest.clone(),
                    format!("{}ms", binding.timeout_ms),
                    binding.max_input_bytes.to_string(),
                    binding
                        .last_actual
                        .as_ref()
                        .map(|actual| {
                            format!(
                                "{}; {} @{}",
                                match actual.status {
                                    RequestCheckStatus::Pending => "pending",
                                    RequestCheckStatus::Interrupted => "interrupted",
                                    RequestCheckStatus::Allowed => "allowed",
                                    RequestCheckStatus::Denied => "denied",
                                    RequestCheckStatus::Failed => "failed",
                                    RequestCheckStatus::NotRun
                                    | RequestCheckStatus::NotEnabled
                                    | RequestCheckStatus::Skipped => "unknown",
                                },
                                match actual.dispatch {
                                    RequestCheckDispatchStatus::NotAttempted => "not_attempted",
                                    RequestCheckDispatchStatus::Attempted => "attempted",
                                    RequestCheckDispatchStatus::ResponseReceived => {
                                        "response_received"
                                    }
                                },
                                actual.observed_at_unix_ms
                            )
                        })
                        .unwrap_or_else(|| "no retained evidence".into()),
                ]);
            }
            human.table(&table)?;
        }
        human.note("Inventory reflects the running daemon; saved configuration state above says whether a restart is required.")
    }
}

impl CliReport for CheckerProbeReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.line("synthetic request-check probe")?;
        human.field("checker", &self.result.checker_id)?;
        human.field("observed", self.result.observed_at_unix_ms)?;
        human.field(
            "reachability",
            match self.result.reachability {
                ProbeReachability::NotAttempted => "not_attempted",
                ProbeReachability::Unknown => "unknown",
                ProbeReachability::Reachable => "reachable",
                ProbeReachability::Unreachable => "unreachable",
            },
        )?;
        human.field(
            "protocol",
            match self.result.protocol {
                ProbeProtocolStatus::NotChecked => "not_checked",
                ProbeProtocolStatus::Incomplete => "incomplete",
                ProbeProtocolStatus::Valid => "valid",
                ProbeProtocolStatus::Invalid => "invalid",
            },
        )?;
        if let Some(latency) = self.result.latency_ms {
            human.field("latency", format!("{latency}ms"))?;
        }
        if let Some(version) = &self.result.implementation_version {
            human.field("implementation", version)?;
        }
        if let Some(decision) = self.result.decision {
            human.field(
                "synthetic decision",
                match decision {
                    CheckerProbeDecision::Allow => "allow",
                    CheckerProbeDecision::Deny => "deny",
                },
            )?;
        }
        if let Some(code) = &self.result.error_code {
            human.field("error", code)?;
        }
        human.field("counts as usage", yes_no(self.counts_as_usage))?;
        human.note("This fixed probe tests reachability and protocol only; actual usage appears only in request receipts.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};
    use bitrouter_sdk::language_model::receipts::{RequestCheckReceipt, RequestReceiptIdentity};
    use bitrouter_sdk::language_model::request_checks::RequestCheckCoverage;

    #[test]
    fn unknown_receipt_never_implies_an_outcome() -> anyhow::Result<()> {
        let lookup = RequestReceiptLookup::Unknown {
            request_id: "request-1".into(),
            requested_incarnation: None,
            current_incarnation: "incarnation-1".into(),
            reason: RequestReceiptUnknownReason::NotRetained,
        };
        let rendered = String::from_utf8(Output::new(Format::Human).render_to_vec(&lookup))?;
        assert!(rendered.contains("unknown"));
        assert!(rendered.contains("not_retained"));
        assert!(rendered.contains("does not prove"));
        Ok(())
    }

    #[test]
    fn unhealthy_receipt_store_is_visible_in_both_views() -> anyhow::Result<()> {
        let report = RequestReceiptList {
            incarnation_id: "incarnation-1".into(),
            capacity: 4096,
            completed_ttl_secs: 900,
            health: RequestReceiptStoreHealth::Unavailable,
            receipts: Vec::new(),
        };
        let human = String::from_utf8(Output::new(Format::Human).render_to_vec(&report))?;
        let json = String::from_utf8(Output::new(Format::Json).render_to_vec(&report))?;
        assert!(human.contains("unavailable"));
        assert!(human.contains("cannot list retained receipts authoritatively"));
        assert!(!human.contains("no retained receipts"));
        assert!(json.contains("\"health\": \"unavailable\""));
        Ok(())
    }

    #[test]
    fn receipt_lookup_renders_failed_checker_evidence_and_coverage() -> anyhow::Result<()> {
        let lookup = RequestReceiptLookup::Found {
            receipt: RequestReceipt {
                identity: RequestReceiptIdentity {
                    receipt_id: "receipt-1".into(),
                    request_id: "request-1".into(),
                    incarnation_id: "incarnation-1".into(),
                    router_id: "coding".into(),
                    router_binding_digest: "sha256:router-binding".into(),
                },
                accepted_at_unix_ms: 1,
                checks: vec![RequestCheckReceipt {
                    contract_version: 1,
                    checker_id: Some("company".into()),
                    binding_digest: Some("sha256:checker-binding".into()),
                    invocation_id: Some("invocation-1".into()),
                    status: RequestCheckStatus::Failed,
                    dispatch: Some(RequestCheckDispatchStatus::ResponseReceived),
                    observed_at_unix_ms: Some(3),
                    reason_code: Some("review_required".into()),
                    implementation_version: Some("checker-v1".into()),
                    failure_kind: Some(CheckerFailureKind::Timeout),
                    coverage: Some(RequestCheckCoverage {
                        scope: RequestCheckCoverageScope::EntryRequestText,
                        text_bytes: 42,
                        text_fragments: 3,
                        excluded_media_fragments: 1,
                        status: RequestCheckCoverageStatus::CompleteWithinScope,
                    }),
                    started_at_unix_ms: Some(2),
                    finished_at_unix_ms: Some(3),
                }],
                upstream_started: false,
                outcome: Some(RequestReceiptOutcome::Failed),
                delivery: RequestDeliveryStatus::NotApplicable,
                failure_stage: Some(RequestFailureStage::RequestCheck),
                completed_at_unix_ms: Some(4),
            },
            retained_matches: 1,
        };

        let rendered = String::from_utf8(Output::new(Format::Human).render_to_vec(&lookup))?;
        for evidence in [
            "failure stage",
            "request_check",
            "check binding",
            "sha256:checker-binding",
            "check invocation",
            "invocation-1",
            "check reason",
            "review_required",
            "check failure",
            "timeout",
            "check implementation",
            "checker-v1",
            "entry_request_text; complete_within_scope; 42 bytes; 3 fragments; 1 media excluded",
        ] {
            assert!(
                rendered.contains(evidence),
                "missing {evidence}: {rendered}"
            );
        }
        Ok(())
    }
}
