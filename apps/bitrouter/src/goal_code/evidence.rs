//! Bounded, owner-scoped evidence reads. A blob alone cannot establish an archive.
use super::*;
use bitrouter_orchestrator::goal::resources::Material;
use bitrouter_orchestrator::goal::workflow::{
    ArtifactRef, CheckState, LocalReconciliation, Observation, ReviewState, archive_operation_id,
};
use bitrouter_tui::goals::Task;

#[cfg(test)]
mod tests;

pub(super) enum Read {
    Text(String),
    Archive {
        detail: Task,
        artifacts: projection::Artifacts,
    },
}

use crate::goal_archive::Record as ArchiveRecord;

pub(super) async fn inspect(
    client: &Client,
    root: &Path,
    goal_id: &str,
    target: &projection::Target,
    permission: bool,
) -> Result<Read> {
    let reference = match target {
        projection::Target::File(reference) | projection::Target::Archive { reference, .. } => {
            reference
        }
    };
    anyhow::ensure!(
        reference.bytes <= 1024 * 1024,
        "evidence exceeds the text preview limit; immutable identity remains available"
    );
    let bytes = crate::goal_artifacts::read(root, reference).await?;
    let projection::Target::Archive { task_id, .. } = target else {
        return Ok(Read::Text(if permission {
            projection::permission_text(&bytes)?
        } else {
            String::from_utf8(bytes).context("binary evidence cannot be shown as text")?
        }));
    };
    let record: ArchiveRecord = serde_json::from_slice(&bytes)?;
    let id = archive_operation_id(goal_id, &record.execution.id);
    let GoalReply::Accepted { operation } = client
        .request(GoalCommand::Operation { operation_id: id })
        .await?
    else {
        anyhow::bail!("archive has no authoritative operation receipt");
    };
    let record = crate::goal_archive::decode(goal_id, task_id, reference, &bytes, &operation)?;
    project(goal_id, task_id, record)
}

fn project(goal_id: &str, task_id: &str, record: ArchiveRecord) -> Result<Read> {
    let execution = &record.execution;
    let mut map = projection::Artifacts::new();
    let mut artifacts = Vec::new();
    if let Some(previous) = &record.previous {
        previous.validate()?;
        artifacts.push(projection::archive_artifact(
            goal_id, task_id, previous, &mut map,
        ));
    }
    let mut add = |label: &str, reference: &ArtifactRef| {
        artifacts.push(projection::artifact(goal_id, label, reference, &mut map));
    };
    if let Observation::Candidate { report, .. } = &execution.observation {
        add("Original worker candidate · unverified by itself", report);
    }
    for check in &execution.checks {
        add(
            &format!("Check {} source workspace", check.spec.id),
            &check.workspace,
        );
        if let CheckState::Observed { report, .. } = &check.state {
            add(&format!("Check {} result", check.spec.id), report);
        }
    }
    if let Some(handoff) = &execution.handoff {
        add("Original checked workspace", &handoff.workspace);
        add("Original semantic handoff review", &handoff.review);
    }
    match &execution.review {
        Some(ReviewState::Requested { workspace, .. }) => add("Review source workspace", workspace),
        Some(ReviewState::Rejected {
            report: Some(report),
            ..
        }) => add("Original review evidence", report),
        _ => {}
    }
    if let Some(LocalReconciliation::Verified {
        workspace,
        transcript,
        ..
    }) = &execution.reconciliation
    {
        add("Stopped local workspace", workspace);
        add("Original tool/stop reconciliation", transcript);
    }
    if let Some(resources) = &execution.resources {
        add("Original prepared prompt", &resources.prompt);
        if let Some(reference) = &resources.checked_dependencies {
            add("Original prepared checked dependency evidence", reference);
        }
        for material in &resources.materials {
            match material {
                Material::Context { path, content } => {
                    add(&format!("Original context {path}"), content)
                }
                Material::Skill { id, name, content } => {
                    add(&format!("Original skill {id} / {name}"), content)
                }
            }
        }
    }
    for decision in &record.decisions {
        add(
            &format!("Retired permission {} · read only", decision.id),
            &decision.request,
        );
    }
    Ok(Read::Archive {
        detail: Task {
            id: task_id.into(),
            title: format!("Archived execution {}", execution.id),
            status: "historical · read only".into(),
            resource: format!("Source Goal requirements {} · task revision {}", execution.requirements_revision, execution.task_revision),
            details: vec![
                "Historical evidence describes this original execution; it does not validate current requirements or authorize work. Esc returns to the pinned task.".into(),
                serde_json::to_string_pretty(execution)?,
                format!("Original retired permissions:\n{}", serde_json::to_string_pretty(&record.decisions)?),
            ],
            artifacts,
            ..Default::default()
        },
        artifacts: map,
    })
}
