//! Human renderers for guarded reload state and retained remote operations.

use crate::output::CliReport;
use crate::output::human::{Health, Human, Table};
use crate::reload::{
    ReloadConsistency, ReloadOutcome, ReloadParticipant, ReloadParticipantOutcome, ReloadReport,
    ReloadState,
};
use crate::remote_control::operations::{OperationReport, OperationStatus};

fn operation_status(status: OperationStatus) -> &'static str {
    match status {
        OperationStatus::Running => "running",
        OperationStatus::Succeeded => "succeeded",
        OperationStatus::Failed => "failed",
        OperationStatus::PartiallyApplied => "partially_applied",
        OperationStatus::Unknown => "unknown",
    }
}

fn operation_health(status: OperationStatus) -> Health {
    match status {
        OperationStatus::Succeeded => Health::Up,
        OperationStatus::Failed | OperationStatus::PartiallyApplied => Health::Down,
        OperationStatus::Running | OperationStatus::Unknown => Health::Unknown,
    }
}

fn outcome(outcome: ReloadOutcome) -> &'static str {
    match outcome {
        ReloadOutcome::Succeeded => "succeeded",
        ReloadOutcome::Failed => "failed",
        ReloadOutcome::PartiallyApplied => "partially_applied",
        ReloadOutcome::Unknown => "unknown",
    }
}

fn participant_outcome(outcome: ReloadParticipantOutcome) -> &'static str {
    match outcome {
        ReloadParticipantOutcome::Applied => "applied",
        ReloadParticipantOutcome::Unchanged => "unchanged",
        ReloadParticipantOutcome::Failed => "failed",
        ReloadParticipantOutcome::NotAttempted => "not_attempted",
    }
}

fn participant(participant: ReloadParticipant) -> &'static str {
    match participant {
        ReloadParticipant::RoutingTable => "routing_table",
        ReloadParticipant::UpstreamTimeoutClients => "upstream_timeout_clients",
        ReloadParticipant::PolicyTable => "policy_table",
        ReloadParticipant::NamedPolicyRuntime => "named_policy_runtime",
        ReloadParticipant::AccessPolicyStore => "access_policy_store",
    }
}

fn consistency(consistency: ReloadConsistency) -> &'static str {
    match consistency {
        ReloadConsistency::Consistent => "consistent",
        ReloadConsistency::Mixed => "mixed",
    }
}

fn render_reload_report(human: &mut Human<'_>, report: &ReloadReport) -> std::io::Result<()> {
    human.field("outcome", outcome(report.outcome))?;
    human.field("generation", report.generation)?;
    human.field("started", &report.started_at)?;
    human.field("completed", &report.completed_at)?;
    if !report.restart_required_fields.is_empty() {
        human.field("restart", report.restart_required_fields.join(", "))?;
    }
    let mut table = Table::new(["PARTICIPANT", "OUTCOME", "DETAIL"]);
    for row in &report.participants {
        table.push([
            participant(row.participant).to_string(),
            participant_outcome(row.outcome).to_string(),
            row.error.as_ref().map_or_else(String::new, |error| {
                format!("{}: {}", error.code, error.message)
            }),
        ]);
    }
    human.table(&table)
}

impl CliReport for OperationReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.status_block(
            operation_health(self.status),
            &format!("reload {}", operation_status(self.status)),
        )?;
        human.field("request", &self.request_id)?;
        human.field("instance", &self.server_instance_id)?;
        human.field("generation", self.generation_before)?;
        human.field("accepted", self.accepted_at_unix_ms)?;
        if let Some(completed) = self.completed_at_unix_ms {
            human.field("completed", completed)?;
        }
        human.field("lookup", &self.lookup_url)?;
        if let Some(report) = &self.result {
            human.blank()?;
            render_reload_report(human, report)?;
        }
        if !self.succeeded() {
            human.note(
                "Inspect this operation and current state before submitting another reload.",
            )?;
        }
        Ok(())
    }

    fn exit_code(&self) -> i32 {
        i32::from(!self.succeeded())
    }
}

impl CliReport for ReloadState {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        let health = if self.running {
            Health::Unknown
        } else if self.consistency == ReloadConsistency::Mixed {
            Health::Down
        } else {
            Health::Up
        };
        human.status_block(health, "reload coordinator state")?;
        human.field("instance", &self.server_instance_id)?;
        human.field("generation", self.generation)?;
        human.field("running", if self.running { "yes" } else { "no" })?;
        if let Some(generation) = self.running_generation {
            human.field("running-gen", generation)?;
        }
        human.field("consistency", consistency(self.consistency))?;
        if let Some(report) = &self.last_outcome {
            human.blank()?;
            render_reload_report(human, report)?;
        }
        if !self.mixed_state_history.is_empty() {
            human.note("Mixed-state history is retained for this daemon boot; inspect the reports before retrying.")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};

    #[test]
    fn running_operation_explains_how_to_recover() -> anyhow::Result<()> {
        let report = OperationReport {
            request_id: "request-id".to_string(),
            server_instance_id: "instance-id".to_string(),
            generation_before: 4,
            status: OperationStatus::Running,
            accepted_at_unix_ms: 1,
            completed_at_unix_ms: None,
            retain_until_unix_ms: None,
            lookup_url: "operations/request-id?instance=instance-id".to_string(),
            result: None,
        };
        let rendered = String::from_utf8(Output::new(Format::Human).render_to_vec(&report))?;
        assert!(rendered.contains("reload running"));
        assert!(rendered.contains("Inspect this operation"));
        assert_eq!(report.exit_code(), 1);
        Ok(())
    }
}
