//! Controlled ACP fixtures exercise the TUI action port and real local IPC.
//! Their manual labels are test inputs, not historical or human review results.

use super::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bitrouter_sdk::acp::capture::{CaptureDirection, CaptureEvent, CaptureKind, CapturePort};
use serde_json::json;

use crate::acp_trajectory::{CanonicalStore, Recorder, RecordingScope, SessionIdentity};
use crate::administration_target::InspectionTarget;
use crate::daemon::{AcpControlPlane, NoopObserveStatus, NoopReloader};
use crate::evolution::evidence::EvidenceKind;
use crate::paths::ConfigSource;

struct Fixture {
    services: CodeServices,
    canonical: CanonicalStore,
    recorder: Arc<Recorder>,
    session: SessionRef,
    evolution: crate::evolution::runtime::EvolutionRuntime,
    routing: Arc<bitrouter_sdk::config::ConfigRoutingTable>,
    server: tokio::task::JoinHandle<Result<()>>,
    _home: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture() -> Result<Fixture> {
    let home = tempfile::tempdir()?;
    let config_path = home.path().join("bitrouter.yaml");
    let config_text = r#"
server:
  skip_auth: true
database:
  url: "sqlite::memory:"
registry:
  inherit_defaults: false
providers:
  fixture:
    api_base: "http://127.0.0.1:1"
    api_key: fixture-only
    models: [{id: judge}, {id: cheap}]
models:
  coding:
    endpoints: [{provider: fixture, service_id: judge}]
  review:
    endpoints: [{provider: fixture, service_id: judge}]
  economy:
    endpoints:
      - {provider: fixture, service_id: cheap}
      - {provider: fixture, service_id: judge}
presets:
  economy:
    model: economy
    params: {temperature: 0.1}
"#;
    tokio::fs::write(&config_path, config_text).await?;
    let config = bitrouter_sdk::config::parse_with(config_text, |_| None)?;
    let assembled = crate::assemble::build_app_with_path(&config, Some(&config_path)).await?;
    let canonical = CanonicalStore::new(assembled.db.clone());
    let recorder = canonical
        .recorder(RecordingScope {
            owner: "local".into(),
            source: "fixture".into(),
            controller_instance_id: None,
            route_scope_id: None,
        })
        .await?;
    for (direction, kind, call_id, method, payload) in [
        (
            CaptureDirection::Client,
            CaptureKind::Request,
            Some(1),
            "session/new",
            json!({"cwd":"/fixture"}),
        ),
        (
            CaptureDirection::Agent,
            CaptureKind::Response,
            Some(1),
            "session/new",
            json!({"result":{"sessionId":"session"}}),
        ),
        (
            CaptureDirection::Client,
            CaptureKind::Request,
            Some(2),
            "session/prompt",
            json!({"sessionId":"session","prompt":[{"type":"text","text":"Fix the parser and run tests. No PR is needed."}]}),
        ),
        (
            CaptureDirection::Agent,
            CaptureKind::Notification,
            None,
            "session/update",
            json!({"sessionId":"session","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"The parser is fixed and tests pass."}}}),
        ),
        (
            CaptureDirection::Agent,
            CaptureKind::Notification,
            None,
            "session/update",
            json!({"sessionId":"session","update":{"sessionUpdate":"tool_call","toolCallId":"checks","title":"Run parser tests","status":"completed","content":[{"type":"content","content":{"type":"text","text":"Controlled fixture: parser tests passed."}}]}}),
        ),
        (
            CaptureDirection::Agent,
            CaptureKind::Response,
            Some(2),
            "session/prompt",
            json!({"result":{"stopReason":"end_turn"}}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction,
                kind,
                call_id,
                method: method.into(),
                payload,
            })
            .await?;
    }
    let socket = home.path().join("evolution.sock");
    let evolution = assembled.evolution.clone();
    let routing = assembled.routing_table.clone();
    let server = tokio::spawn(crate::daemon::run_control_socket_with_acp_runtime(
        socket.clone(),
        Arc::new(assembled.app),
        "127.0.0.1:1".into(),
        Arc::new(NoopReloader),
        Arc::new(NoopObserveStatus { compiled_in: false }),
        AcpControlPlane {
            runtime: assembled.acp_runtime,
            metering: crate::metering::MeteringStore::new(assembled.db),
            inventory: Some(assembled.evolution.inventory()),
            evolution: Some(assembled.evolution),
        },
    ));
    let fixture = Fixture {
        services: CodeServices {
            target: InspectionTarget::Local {
                source: ConfigSource::File(config_path),
                socket: socket.clone(),
            },
            label: "Controlled fixture".into(),
            operations_only: false,
            can_reload: false,
            initial_launch: Mutex::new(None),
        },
        canonical,
        recorder,
        evolution,
        routing,
        session: SessionRef {
            source: "fixture".into(),
            session_id: "session".into(),
        },
        server,
        _home: home,
    };
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fixture.services.evolution_status().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    Ok(fixture)
}

mod candidates;
mod history;

#[tokio::test]
async fn judge_cost_status_over_local_ipc_keeps_missing_fees_unknown() -> Result<()> {
    use crate::evolution::jobs::{JudgeJob, JudgeJobs};
    let fixture = fixture().await?;
    let draft = fixture.review().await?;
    let store = fixture.evolution.service("local")?.store;
    let jobs = JudgeJobs::new(store.clone());
    let job = jobs
        .enqueue(
            &fixture.identity(),
            &draft.input.checkpoint.checkpoint_id,
            "fixture:judge",
            None,
        )
        .await?;
    let (revision, _): (_, JudgeJob) = store
        .get("judge_job", &job.job_id)
        .await?
        .context("job missing")?;
    // A historical job can have an attempted request with no metering or
    // dispatch inventory. The UI must not mistake its stored zero subtotal
    // for an observed free evaluation.
    store
        .update("judge_job", &job.job_id, revision, |job: &mut JudgeJob| {
            job.request_ids
                .push("legacy-unmetered-judge-request".into());
            Ok(())
        })
        .await?;
    let status = fixture.services.evolution_status().await?;
    assert_eq!(status.judge_costs.summary.total_cost_micro_usd, None);
    assert_eq!(status.judge_costs.summary.incomplete_requests, 1);
    let panel = fixture.step("evolution:menu", "status", None).await?;
    let content = panel.inspector.context("status inspector missing")?.content;
    assert!(content.contains("$0.000000 known metered subtotal; total unknown"));
    assert!(content.contains("Historical dispatch details are unavailable"));
    assert!(content.contains("does not establish net routing savings"));
    assert!(panel.selector.is_some());
    Ok(())
}

impl Fixture {
    fn identity(&self) -> SessionIdentity {
        SessionIdentity {
            owner: "local".into(),
            source: self.session.source.clone(),
            native_session_id: self.session.session_id.clone(),
        }
    }

    async fn step(
        &self,
        selector: &str,
        choice: &str,
        draft: Option<ReviewDraft>,
    ) -> Result<Panel> {
        self.services
            .evolution_step(selector, choice, Some(self.session.clone()), draft, None)
            .await
    }

    async fn review(&self) -> Result<ReviewDraft> {
        let panel = self.step("evolution:menu", "checkpoints", None).await?;
        let selector = panel.selector.context("checkpoint selector missing")?;
        let freeze = selector.rows.first().context("freeze action missing")?;
        self.step(&selector.id, &freeze.id, None)
            .await?
            .review
            .context("review missing")
    }
}

fn edit(draft: ReviewDraft, selector: &str, choice: &str) -> Result<ReviewDraft> {
    draft
        .step(selector, choice)?
        .review
        .context("edited draft missing")
}

fn scored(mut draft: ReviewDraft) -> Result<ReviewDraft> {
    let evidence = draft
        .input
        .evidence
        .items
        .iter()
        .position(|item| item.kind == EvidenceKind::ToolObservation)
        .context("fixture tool result missing")?;
    for index in 0..draft.evaluation.items.len() {
        draft = edit(
            draft,
            &format!("evolution:score:{index}"),
            if index < 3 { "1" } else { "na" },
        )?;
        draft = edit(
            draft,
            &format!("evolution:reason:{index}"),
            if index < 3 {
                "Controlled test input: the cited result supports this score."
            } else {
                "Controlled test input: this task has no such obligation."
            },
        )?;
        if index < 3 {
            draft = edit(
                draft,
                &format!("evolution:citations:{index}"),
                &evidence.to_string(),
            )?;
        }
    }
    edit(
        draft,
        "evolution:summary",
        "Controlled manual assessment for the recorded fixture.",
    )
}

#[tokio::test]
async fn tui_manual_review_roundtrips_over_ipc_and_rejects_stale_corrections() -> Result<()> {
    let fixture = fixture().await?;
    let draft = scored(fixture.review().await?)?;
    let disconnected = fixture
        .services
        .evolution_step("evolution:menu", "mode", None, Some(draft.clone()), None)
        .await?;
    assert_eq!(
        disconnected.selector.as_ref().map(|s| s.id.as_str()),
        Some("evolution:mode")
    );
    let preview = draft.clone().step("evolution:review", "preview")?;
    assert!(
        preview
            .inspector
            .as_ref()
            .is_some_and(|i| i.content.contains("Quality: 1.00–1.00"))
    );
    assert_eq!(
        preview.selector.as_ref().map(|s| s.id.as_str()),
        Some("evolution:submit")
    );
    let saved = fixture
        .step("evolution:submit", "save", Some(draft.clone()))
        .await?;
    assert!(saved.clear_drafts);
    let effective = fixture
        .canonical
        .effective_assessment(&fixture.identity())
        .await?;
    let revision = effective
        .current_revision
        .context("manual revision missing")?;
    assert!(effective.assessment.is_some_and(
        |r| r.input.source == crate::acp_trajectory::checkpoint::types::AssessmentSource::Human
    ));
    // A lost IPC reply can be retried without creating a second revision.
    fixture
        .step("evolution:submit", "save", Some(draft.clone()))
        .await?;
    assert_eq!(
        fixture
            .canonical
            .assessment_history(&fixture.identity())
            .await?
            .len(),
        1
    );
    let changed = edit(
        draft,
        "evolution:summary",
        "A stale local draft must not overwrite the selected assessment.",
    )?;
    assert!(
        fixture
            .step("evolution:submit", "save", Some(changed))
            .await
            .is_err()
    );
    assert_eq!(
        fixture
            .canonical
            .effective_assessment(&fixture.identity())
            .await?
            .current_revision
            .as_deref(),
        Some(revision.as_str())
    );
    let reopened = fixture.review().await?;
    assert!(reopened.input.previous.is_some());
    assert_eq!(
        reopened.input.expected_revision.as_deref(),
        Some(revision.as_str())
    );
    assert!(fixture.services.evolution_status().await?.jobs.is_empty());
    Ok(())
}

#[tokio::test]
async fn manual_ui_preserves_unknowns_and_requires_original_verification_evidence() -> Result<()> {
    let fixture = fixture().await?;
    let raw = fixture.review().await?;
    assert!(raw.clone().step("evolution:score:0", "na").is_err());
    assert!(raw.clone().step("evolution:score:0", "NaN").is_err());
    assert!(raw.clone().step("evolution:review", "preview").is_err());
    let read = raw.clone().step("evolution:review", "evidence")?;
    assert_eq!(
        read.selector.as_ref().map(|s| s.id.as_str()),
        Some("evolution:review")
    );
    assert!(read.inspector.is_some());
    let mut draft = scored(raw)?;
    let agent = draft
        .input
        .evidence
        .items
        .iter()
        .position(|item| item.kind == EvidenceKind::AgentMessage)
        .context("agent fixture missing")?;
    draft.evaluation.items[2].evidence = vec![draft.input.evidence.items[agent].citation.clone()];
    assert!(draft.clone().step("evolution:review", "preview").is_err());
    draft = edit(draft, "evolution:score:2", "unknown")?;
    let submission = draft.submission()?;
    let bounds = submission.evaluation.aggregate(&draft.input.evidence)?;
    assert!(bounds.lower_ppm < bounds.upper_ppm);
    assert!(bounds.complete_score().is_none());
    assert!(fixture.services.evolution_status().await?.jobs.is_empty());
    Ok(())
}

#[tokio::test]
async fn continued_prefix_is_saved_with_explicit_stale_status() -> Result<()> {
    let fixture = fixture().await?;
    let draft = scored(fixture.review().await?)?;
    fixture.recorder.record(CaptureEvent { direction: CaptureDirection::Client, kind: CaptureKind::Request, call_id: Some(3), method: "session/prompt".into(), payload: json!({"sessionId":"session","prompt":[{"type":"text","text":"Continue with another change."}]}) }).await?;
    let result = fixture
        .step("evolution:submit", "save", Some(draft))
        .await?;
    assert!(
        result
            .inspector
            .is_some_and(|i| i.content.contains("excluded from current learning"))
    );
    let effective = fixture
        .canonical
        .effective_assessment(&fixture.identity())
        .await?;
    assert!(effective.stale);
    assert!(
        effective
            .checkpoint
            .is_some_and(|cp| cp.watermark < effective.current_watermark)
    );
    Ok(())
}

#[tokio::test]
async fn judge_model_selection_preserves_off_and_remote_operations_have_no_controls() -> Result<()>
{
    let mut fixture = fixture().await?;
    fixture
        .step("evolution:judge", "fixture:judge", None)
        .await?;
    let status = fixture.services.evolution_status().await?;
    assert_eq!(status.control.mode, EvolutionMode::Off);
    assert_eq!(status.control.judge_model.as_deref(), Some("fixture:judge"));
    fixture.step("evolution:mode", "manual", None).await?;
    assert_eq!(
        fixture.services.evolution_status().await?.control.mode,
        EvolutionMode::Manual
    );
    fixture.step("evolution:mode", "automatic", None).await?;
    assert_eq!(
        fixture.services.evolution_status().await?.control.mode,
        EvolutionMode::Automatic
    );
    fixture.step("evolution:mode", "off", None).await?;
    fixture
        .step("evolution:judge", "fixture:judge", None)
        .await?;
    assert_eq!(
        fixture.services.evolution_status().await?.control.mode,
        EvolutionMode::Off
    );
    assert!(
        fixture
            .step("evolution:judge", "missing", None)
            .await
            .is_err()
    );
    fixture.services.operations_only = true;
    assert!(!fixture.services.evolution_available());
    assert!(fixture.step("evolution:menu", "open", None).await.is_err());
    Ok(())
}
