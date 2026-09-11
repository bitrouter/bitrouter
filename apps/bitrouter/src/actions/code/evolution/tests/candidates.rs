use super::*;
use crate::evolution::bandit::Arm;
use crate::evolution::runtime::candidates::{CandidateAction, CandidateReport, FeedbackChoice};
use crate::evolution::service::DecisionContext;

impl Fixture {
    async fn candidate_step(
        &self,
        selector: &str,
        choice: &str,
        draft: Option<CandidateDraft>,
    ) -> Result<Panel> {
        self.services
            .evolution_step(selector, choice, Some(self.session.clone()), None, draft)
            .await
    }

    async fn candidate(&self) -> Result<CandidateDraft> {
        let mut draft = self
            .candidate_step("evolution:menu", "candidate", None)
            .await?
            .candidate
            .context("candidate draft missing")?;
        for (selector, choice) in [
            ("evolution:candidate:name", "cost-trial"),
            ("evolution:candidate:baseline", "coding"),
            ("evolution:candidate:target", "@economy"),
            ("evolution:candidate:baseline", "review"),
            ("evolution:candidate:target", "fixture:cheap"),
            (
                "evolution:candidate:rationale",
                "Compare complete cost while preserving delivery quality",
            ),
            (
                "evolution:candidate:independence",
                "Coder and reviewer changes are grouped; no other active blocks",
            ),
        ] {
            draft = self
                .candidate_step(selector, choice, Some(draft))
                .await?
                .candidate
                .context("updated candidate missing")?;
        }
        Ok(draft)
    }

    async fn preview_candidate(&self, draft: CandidateDraft) -> Result<CandidateDraft> {
        self.candidate_step("evolution:candidate:form", "preview", Some(draft))
            .await?
            .candidate
            .context("reviewed candidate missing")
    }
}

#[tokio::test]
async fn tui_operator_restore_reviews_submits_retries_and_exposes_publication_history() -> Result<()>
{
    let fixture = fixture().await?;
    let draft = fixture
        .preview_candidate(fixture.candidate().await?)
        .await?;
    fixture
        .candidate_step("evolution:candidate:submit", "register", Some(draft))
        .await?;
    let evidence = fixture.step("evolution:block", "cost-trial", None).await?;
    let progress = &evidence
        .inspector
        .as_ref()
        .context("evidence view missing")?
        .content;
    assert!(progress.contains("Evidence minimum: 20 session groups per arm"));
    assert!(progress.contains("Comparable session groups: quality 0, cost 0, duration 0"));
    assert!(progress.contains("Reaching this count does not by itself permit adoption"));
    let actions = evidence.selector.context("actions missing")?;
    let restore = actions
        .rows
        .iter()
        .find(|row| row.id.starts_with("restore:"))
        .context("restore action missing")?;
    let reason = fixture
        .step(&actions.id, &restore.id, None)
        .await?
        .selector
        .context("reason entry missing")?;
    assert!(fixture.step(&reason.id, "  ", None).await.is_err());
    let reviewed = fixture
        .step(&reason.id, "The candidate does not suit my workflow", None)
        .await?;
    let preview = reviewed.inspector.context("review missing")?.content;
    assert!(preview.contains("coding → coding"));
    assert!(preview.contains("review → review"));
    assert!(preview.contains("The candidate does not suit my workflow"));
    let confirm = reviewed.selector.context("confirmation missing")?;
    let submit = confirm.rows.first().context("submit action missing")?;
    let receipt = fixture
        .step(&confirm.id, &submit.id, None)
        .await?
        .inspector
        .context("receipt missing")?;
    assert!(receipt.content.contains("Withdrawal recorded:"));
    let status = fixture.services.evolution_status().await?;
    assert_eq!(status.control.mode, EvolutionMode::Off);
    assert_eq!(
        status
            .control
            .blocks
            .get("cost-trial")
            .context("block missing")?
            .status,
        crate::evolution::control::BlockStatus::RolledBack
    );
    fixture.step(&confirm.id, &submit.id, None).await?;
    assert_eq!(
        fixture
            .services
            .evolution_status()
            .await?
            .control
            .generation,
        status.control.generation
    );
    let evidence = fixture.step("evolution:block", "cost-trial", None).await?;
    let history = evidence.inspector.context("history missing")?.content;
    assert!(history.contains("Operator restored the supported baseline"));
    assert!(history.contains("The candidate does not suit my workflow"));
    assert!(
        evidence
            .selector
            .context("actions missing")?
            .rows
            .iter()
            .all(|row| !row.id.starts_with("restore:"))
    );
    Ok(())
}

#[tokio::test]
async fn tui_operator_restore_does_not_rebase_a_stale_confirmation() -> Result<()> {
    let fixture = fixture().await?;
    let draft = fixture
        .preview_candidate(fixture.candidate().await?)
        .await?;
    fixture
        .candidate_step("evolution:candidate:submit", "register", Some(draft))
        .await?;
    let actions = fixture
        .step("evolution:block", "cost-trial", None)
        .await?
        .selector
        .context("actions missing")?;
    let restore = actions
        .rows
        .iter()
        .find(|row| row.id.starts_with("restore:"))
        .context("restore action missing")?;
    let reason = fixture
        .step(&actions.id, &restore.id, None)
        .await?
        .selector
        .context("reason entry missing")?;
    let confirm = fixture
        .step(&reason.id, "Stop the reviewed trial", None)
        .await?
        .selector
        .context("confirmation missing")?;
    let original = fixture
        .services
        .evolution_status()
        .await?
        .control
        .blocks
        .get("cost-trial")
        .context("block missing")?
        .clone();
    let mut next = original.definition;
    next.rules[0].challenger_route = "fixture:cheap".into();
    fixture
        .evolution
        .revise("local", next, original.experiment_id)
        .await?;
    let before = fixture.services.evolution_status().await?.control;
    let submit = confirm.rows.first().context("submit action missing")?;
    let error = fixture
        .step(&confirm.id, &submit.id, None)
        .await
        .err()
        .context("stale withdrawal succeeded")?;
    assert!(error.to_string().contains("changed"));
    assert_eq!(
        serde_json::to_value(fixture.services.evolution_status().await?.control)?,
        serde_json::to_value(before)?
    );
    Ok(())
}

#[tokio::test]
async fn candidate_revision_and_history_use_the_local_ipc_with_stable_receipts() -> Result<()> {
    let fixture = fixture().await?;
    let draft = fixture
        .preview_candidate(fixture.candidate().await?)
        .await?;
    let initial_preview = draft.preview.clone().context("missing initial preview")?;
    fixture
        .candidate_step("evolution:candidate:submit", "register", Some(draft))
        .await?;
    let status = fixture.services.evolution_status().await?;
    let original = status
        .control
        .blocks
        .get("cost-trial")
        .context("missing original")?
        .clone();
    let mut draft = fixture
        .candidate_step("evolution:revise", "cost-trial", None)
        .await?
        .candidate
        .context("missing revision draft")?;
    assert_eq!(
        draft.spec.predecessor.as_deref(),
        Some(original.experiment_id.as_str())
    );
    assert_eq!(draft.spec.rules.len(), 2);
    assert!(
        draft
            .menu()
            .rows
            .iter()
            .all(|row| row.id != "add" && row.id != "name" && !row.id.starts_with("remove:"))
    );
    assert!(
        fixture
            .candidate_step(
                "evolution:candidate:name",
                "different-name",
                Some(draft.clone())
            )
            .await
            .is_err()
    );
    for (selector, choice) in [
        ("evolution:candidate:form", "change:0"),
        ("evolution:candidate:target", "fixture:cheap"),
        (
            "evolution:candidate:rationale",
            "Try a simpler compatible route in the next experiment",
        ),
    ] {
        draft = fixture
            .candidate_step(selector, choice, Some(draft))
            .await?
            .candidate
            .context("missing changed draft")?;
    }
    let draft = fixture.preview_candidate(draft).await?;
    let preview = draft.preview.clone().context("missing revision preview")?;
    assert!(!preview.reset_to_configured);
    fixture
        .candidate_step("evolution:candidate:submit", "register", Some(draft))
        .await?;
    let status = fixture.services.evolution_status().await?;
    let latest = status
        .control
        .blocks
        .get("cost-trial")
        .context("missing current")?;
    assert_ne!(latest.experiment_id, original.experiment_id);
    assert_eq!(latest.revision, original.revision);
    assert_eq!(status.control.archived_experiments.len(), 1);
    assert_eq!(status.control.mode, EvolutionMode::Off);
    let history = fixture
        .candidate_step("evolution:menu", "history", None)
        .await?
        .selector
        .context("missing history")?;
    let row = history.rows.first().context("missing archived row")?;
    let evidence = fixture
        .candidate_step("evolution:history", &row.id, None)
        .await?;
    assert!(
        evidence
            .inspector
            .context("missing history evidence")?
            .content
            .contains("Archived experiment: no new enrollments")
    );
    let selected: (String, String) = serde_json::from_str(
        &evidence
            .selector
            .context("missing reconciliation menu")?
            .rows[0]
            .id,
    )?;
    assert_eq!(
        selected,
        ("cost-trial".into(), original.experiment_id.clone())
    );
    for (preview, expected) in [
        (initial_preview, original.experiment_id.clone()),
        (preview, latest.experiment_id.clone()),
    ] {
        let CandidateReport::Registered { experiment_id, .. } = fixture
            .services
            .candidate_request(CandidateAction::Register {
                preview: Box::new(preview),
            })
            .await?
        else {
            anyhow::bail!("missing registration receipt")
        };
        assert_eq!(experiment_id, expected);
    }
    assert_eq!(
        fixture
            .services
            .evolution_status()
            .await?
            .control
            .generation,
        status.control.generation
    );
    Ok(())
}

#[tokio::test]
async fn revision_preview_rebases_changed_routes_and_fences_a_later_reload() -> Result<()> {
    let fixture = fixture().await?;
    let draft = fixture
        .preview_candidate(fixture.candidate().await?)
        .await?;
    fixture
        .candidate_step("evolution:candidate:submit", "register", Some(draft))
        .await?;
    let mut config = fixture.routing.snapshot_config();
    config
        .presets
        .get_mut("economy")
        .context("missing preset")?
        .system_prompt = Some("Changed prompt defaults".into());
    fixture
        .routing
        .replace_prepared_config(config.clone())
        .await?;
    let mut draft = fixture
        .candidate_step("evolution:revise", "cost-trial", None)
        .await?
        .candidate
        .context("missing revision")?;
    draft.spec.rationale = "Revalidate after a configured route changed".into();
    let draft = fixture.preview_candidate(draft).await?;
    assert!(
        draft
            .preview
            .as_ref()
            .context("missing preview")?
            .reset_to_configured
    );
    assert!(
        draft
            .spec
            .rules
            .iter()
            .all(|rule| rule.baseline_route == rule.selector)
    );
    config
        .presets
        .get_mut("economy")
        .context("missing preset")?
        .system_prompt = Some("Another change".into());
    fixture.routing.replace_prepared_config(config).await?;
    assert!(
        fixture
            .candidate_step(
                "evolution:candidate:submit",
                "register",
                Some(draft.clone())
            )
            .await
            .is_err()
    );
    let fresh = fixture.preview_candidate(draft).await?;
    fixture
        .candidate_step("evolution:candidate:submit", "register", Some(fresh))
        .await?;
    let status = fixture.services.evolution_status().await?;
    assert_eq!(status.control.archived_experiments.len(), 1);
    assert!(
        status
            .control
            .blocks
            .get("cost-trial")
            .context("missing current")?
            .baseline_ancestry
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn candidate_creation_registers_reviewed_joint_rules_and_retries_without_mode_changes()
-> Result<()> {
    let fixture = fixture().await?;
    let draft = fixture.candidate().await?;
    assert_eq!(draft.spec.rules.len(), 2);
    assert!(matches!(draft.spec.feedback, FeedbackChoice::Manual));
    let panel = fixture
        .candidate_step("evolution:candidate:form", "preview", Some(draft))
        .await?;
    let inspector = panel.inspector.context("preview inspector missing")?;
    let serialized = inspector.content;
    assert!(serialized.contains("Hop 2: fixture/judge"));
    assert!(serialized.contains("10.0%"));
    assert!(!serialized.contains("fixture-only"));
    assert!(!serialized.contains("127.0.0.1"));
    let draft = panel.candidate.context("preview draft missing")?;
    let preview = draft.preview.as_ref().context("preview missing")?;
    let manual = scored(fixture.review().await?)?.submission()?;
    assert_eq!(
        preview.definition.measurement_contract,
        crate::evolution::scoring::measurement_contract(
            manual.source,
            &manual.evaluator_id,
            &manual.evaluator_version
        )?
    );
    assert!(
        fixture
            .services
            .evolution_status()
            .await?
            .control
            .blocks
            .is_empty()
    );
    let saved = fixture
        .candidate_step(
            "evolution:candidate:submit",
            "register",
            Some(draft.clone()),
        )
        .await?;
    assert!(saved.clear_drafts);
    let first = fixture.services.evolution_status().await?;
    assert_eq!(first.control.mode, EvolutionMode::Off);
    assert_eq!(first.control.blocks.len(), 1);
    assert!(first.jobs.is_empty());
    let registered = first
        .control
        .blocks
        .get("cost-trial")
        .context("registered block missing")?;
    assert_eq!(registered.definition.rules.len(), 2);
    let experiment = registered.experiment_id.clone();
    fixture.step("evolution:mode", "manual", None).await?;
    let enabled = fixture.services.evolution_status().await?;
    // The original acknowledgement is replayable even after later mode changes.
    fixture
        .candidate_step("evolution:candidate:submit", "register", Some(draft))
        .await?;
    let retry = fixture.services.evolution_status().await?;
    assert_eq!(retry.control.generation, enabled.control.generation);
    assert_eq!(retry.control.mode, EvolutionMode::Manual);
    assert_eq!(
        retry
            .control
            .blocks
            .get("cost-trial")
            .context("block missing")?
            .experiment_id,
        experiment
    );

    let identity = SessionIdentity {
        owner: "local".into(),
        source: "fixture".into(),
        native_session_id: "fresh-trial".into(),
    };
    for (kind, payload) in [
        (CaptureKind::Request, json!({"cwd":"/fixture"})),
        (
            CaptureKind::Response,
            json!({"result":{"sessionId":identity.native_session_id}}),
        ),
    ] {
        fixture
            .recorder
            .record(CaptureEvent {
                direction: if kind == CaptureKind::Request {
                    CaptureDirection::Client
                } else {
                    CaptureDirection::Agent
                },
                kind,
                call_id: Some(21),
                method: "session/new".into(),
                payload,
            })
            .await?;
    }
    let service = fixture.evolution.service("local")?;
    let dependency = retry
        .control
        .blocks
        .get("cost-trial")
        .context("block missing")?
        .routing_config_digest
        .clone();
    let intent = service
        .select(
            &identity,
            "new-route-intent",
            DecisionContext {
                selector: "coding".into(),
                fingerprint: "fixture".into(),
            },
            &std::collections::BTreeMap::from([("cost-trial".into(), dependency)]),
        )
        .await?
        .context("intent missing")?;
    let enrollment = service
        .enrollment(&identity)
        .await?
        .context("enrollment missing")?;
    let assignment = enrollment
        .assignments
        .get("cost-trial")
        .context("new session not admitted")?;
    assert_eq!(
        intent.selected_route,
        if assignment.arm == Arm::Challenger {
            "@economy"
        } else {
            "coding"
        }
    );
    Ok(())
}

#[tokio::test]
async fn candidate_preview_fences_mode_routes_and_owner_and_preserves_recovery() -> Result<()> {
    let fixture = fixture().await?;
    let draft = fixture
        .preview_candidate(fixture.candidate().await?)
        .await?;
    let original = draft.preview.as_ref().context("preview missing")?.clone();
    assert!(
        fixture
            .evolution
            .candidate_action(
                "another-owner",
                CandidateAction::Register {
                    preview: Box::new(original.clone())
                }
            )
            .await
            .is_err()
    );
    let mut tampered = original;
    tampered.spec.rules[0].challenger_route = "fixture:judge".into();
    assert!(
        fixture
            .services
            .candidate_request(CandidateAction::Register {
                preview: Box::new(tampered)
            })
            .await
            .is_err()
    );
    fixture.step("evolution:mode", "manual", None).await?;
    assert!(
        fixture
            .candidate_step(
                "evolution:candidate:submit",
                "register",
                Some(draft.clone())
            )
            .await
            .is_err()
    );
    assert!(
        fixture
            .services
            .evolution_status()
            .await?
            .control
            .blocks
            .is_empty()
    );
    let reviewed = fixture.preview_candidate(draft).await?;
    let (_, mut config) = fixture.routing.versioned_snapshot();
    config
        .providers
        .get_mut("fixture")
        .context("provider missing")?
        .api_base = "http://127.0.0.1:2".into();
    fixture.routing.replace_prepared_config(config).await?;
    assert!(
        fixture
            .candidate_step(
                "evolution:candidate:submit",
                "register",
                Some(reviewed.clone())
            )
            .await
            .is_err()
    );
    assert!(
        fixture
            .services
            .evolution_status()
            .await?
            .control
            .blocks
            .is_empty()
    );
    let refreshed = fixture.preview_candidate(reviewed).await?;
    fixture
        .candidate_step("evolution:candidate:submit", "register", Some(refreshed))
        .await?;
    assert_eq!(
        fixture
            .services
            .evolution_status()
            .await?
            .control
            .blocks
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn candidate_catalog_refreshes_evaluator_and_rejects_overlap_and_invalid_routes() -> Result<()>
{
    let fixture = fixture().await?;
    let mut draft = fixture.candidate().await?;
    fixture
        .step("evolution:judge", "fixture:judge", None)
        .await?;
    draft = fixture
        .candidate_step("evolution:candidate:form", "feedback", Some(draft))
        .await?
        .candidate
        .context("draft missing")?;
    draft = fixture
        .candidate_step("evolution:candidate:feedback", "judge", Some(draft))
        .await?
        .candidate
        .context("draft missing")?;
    let reviewed = fixture.preview_candidate(draft.clone()).await?;
    assert_eq!(
        reviewed
            .preview
            .as_ref()
            .context("preview missing")?
            .definition
            .measurement_contract,
        crate::evolution::scoring::measurement_contract(
            crate::acp_trajectory::checkpoint::types::AssessmentSource::Agentic,
            "checkpoint-judge:fixture:judge",
            crate::evolution::judge::JUDGE_VERSION
        )?
    );
    fixture
        .step("evolution:judge", "fixture:cheap", None)
        .await?;
    assert!(fixture.preview_candidate(draft.clone()).await.is_err());
    draft = fixture
        .candidate_step("evolution:candidate:form", "feedback", Some(draft))
        .await?
        .candidate
        .context("draft missing")?;
    draft = fixture
        .candidate_step("evolution:candidate:feedback", "judge", Some(draft))
        .await?
        .candidate
        .context("draft missing")?;
    let good = fixture.preview_candidate(draft.clone()).await?;
    let mut invalid = draft.clone();
    invalid.spec.rules[0].challenger_route = "unknown-provider:missing".into();
    assert!(fixture.preview_candidate(invalid).await.is_err());
    fixture
        .candidate_step("evolution:candidate:submit", "register", Some(good))
        .await?;
    let mut overlap = draft;
    overlap.spec.block_id = "overlapping-trial".into();
    assert!(fixture.preview_candidate(overlap).await.is_err());
    let CandidateReport::Catalog(catalog) = fixture
        .services
        .candidate_request(CandidateAction::Catalog)
        .await?
    else {
        bail!("catalog missing")
    };
    assert_eq!(catalog.reserved_matchers.len(), 2);
    assert!(fixture.services.evolution_status().await?.jobs.is_empty());
    Ok(())
}

#[tokio::test]
async fn candidate_draft_survives_disconnect_but_cannot_mix_with_other_session_or_review()
-> Result<()> {
    let fixture = fixture().await?;
    let draft = fixture.candidate().await?;
    let resumed = fixture
        .services
        .evolution_step(
            "evolution:menu",
            "resume_candidate",
            None,
            None,
            Some(draft.clone()),
        )
        .await?;
    assert_eq!(
        resumed.selector.as_ref().map(|s| s.id.as_str()),
        Some("evolution:candidate:form")
    );
    let different = SessionRef {
        source: "fixture".into(),
        session_id: "different".into(),
    };
    assert!(
        fixture
            .services
            .evolution_step(
                "evolution:menu",
                "resume_candidate",
                Some(different),
                None,
                Some(draft.clone())
            )
            .await
            .is_err()
    );
    let review = fixture.review().await?;
    assert!(
        fixture
            .services
            .evolution_step(
                "evolution:menu",
                "open",
                None,
                Some(review),
                Some(draft.clone())
            )
            .await
            .is_err()
    );
    let discarded = fixture
        .candidate_step("evolution:candidate:form", "discard", Some(draft))
        .await?;
    assert!(discarded.clear_drafts);
    assert!(
        fixture
            .services
            .evolution_status()
            .await?
            .control
            .blocks
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn candidate_preview_lists_balanced_accounts_without_credentials() -> Result<()> {
    let fixture = fixture().await?;
    let draft = fixture.candidate().await?;
    let (_, mut config) = fixture.routing.versioned_snapshot();
    let provider = config
        .providers
        .get_mut("fixture")
        .context("provider missing")?;
    provider.account_strategy = bitrouter_sdk::config::AccountStrategy::Balance;
    provider.accounts = vec![
        bitrouter_sdk::config::ProviderAccount {
            label: "first".into(),
            api_key: "first-account-secret".into(),
            ..Default::default()
        },
        bitrouter_sdk::config::ProviderAccount {
            label: "second".into(),
            api_key: "second-account-secret".into(),
            ..Default::default()
        },
    ];
    fixture.routing.replace_prepared_config(config).await?;
    let first = fixture
        .preview_candidate(draft.clone())
        .await?
        .preview
        .context("preview missing")?;
    let second = fixture
        .preview_candidate(draft)
        .await?
        .preview
        .context("preview missing")?;
    assert_eq!(first.preview_digest, second.preview_digest);
    let descriptions = first
        .route_descriptions
        .values()
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    assert!(descriptions.contains("account first"));
    assert!(descriptions.contains("account second"));
    assert!(descriptions.contains("order may rotate"));
    assert!(!descriptions.contains("first-account-secret"));
    assert!(!descriptions.contains("second-account-secret"));
    assert!(!descriptions.contains("fixture-only"));
    assert!(!descriptions.contains("127.0.0.1"));
    Ok(())
}
