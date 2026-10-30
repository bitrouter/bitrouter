use super::*;
use crate::gateway::model::decision::types::DecisionUsage;
use crate::storage::checkpoint::snapshot::context::WorkUnit;
use crate::storage::checkpoint::snapshot::context::evidence::{
    EvidenceBlock, EvidenceSource, SourceSpan,
};
use bitrouter_ai::harness::inspection::AgentStatus;

struct Fixture {
    store: ContextStore,
    manifest: HarnessManifest,
    prompt: Prompt,
    policy: DecisionPolicy,
    skill: String,
    optional: String,
}

impl Fixture {
    fn new() -> Result<Self, CoreError> {
        let manifest = HarnessManifest {
            tools: Vec::new(),
            tool_manifest_digest: "a".repeat(64),
            workspace_id: "workspace".into(),
            workspace_revision: Some("revision".into()),
            permission_revision: 7,
            max_tool_output_bytes: 1024,
            artifact_quota_bytes: 4096,
            max_artifact_chunk_bytes: 1024,
            required_features: vec![crate::storage::checkpoint::snapshot::context::FEATURE.into()],
        };
        let source = EvidenceSource::from_manifest("task", "agent", &manifest);
        let mut skill_source = source.clone();
        skill_source.skill_binding = Some("approved-skill-binding".into());
        // Real skill deliveries are protected User messages. An otherwise
        // optional synthetic group isolates the additional binding safeguard.
        let skill = EvidenceBlock::new(
            vec![Message::text(Role::Assistant, "bound skill evidence")],
            skill_source,
        )?;
        assert!(!skill.protected);
        let optional = EvidenceBlock::new(
            vec![Message::text(Role::Assistant, "unrelated archive")],
            source,
        )?;
        let prompt = Prompt {
            model: "generation".into(),
            system: Some("Required instructions and verified managed skills".into()),
            system_provider_metadata: Default::default(),
            messages: [skill.messages.clone(), optional.messages.clone()].concat(),
            tools: Vec::new(),
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        };
        let mut store = ContextStore::default();
        let skill = store.insert_block(skill)?;
        let optional = store.insert_block(optional)?;
        store.work.insert(
            "task".into(),
            WorkUnit {
                task_id: "task".into(),
                run_id: "run".into(),
                agent_id: "agent".into(),
                parent_task_id: None,
                text: "Implement the current task".into(),
                acceptance_criteria: Vec::new(),
                instructions: vec!["Keep required instructions".into()],
                evidence: vec![skill.clone(), optional.clone()],
                shared_evidence: Vec::new(),
                recalled: Vec::new(),
                last_view_id: None,
                status: AgentStatus::Runnable,
                result_evidence: Vec::new(),
                skills: None,
            },
        );
        Ok(Self {
            store,
            manifest,
            prompt,
            policy: DecisionPolicy {
                retain_recent_groups: 0,
                target_context_bytes: 1,
                ..Default::default()
            },
            skill,
            optional,
        })
    }

    fn candidates(&self) -> Result<CandidateSet, CoreError> {
        CandidateSet::prepare(
            &self.store,
            "task",
            vec![self.skill.clone(), self.optional.clone()],
            &self.manifest,
            &self.policy,
        )
    }

    fn source(&self) -> Result<SourceRevision, CoreError> {
        SourceRevision::capture(1, 1, &self.manifest, &self.prompt)
    }
}

#[test]
fn skill_binding_is_required_and_never_offered_for_choice() -> Result<(), CoreError> {
    let fixture = Fixture::new()?;
    let mut candidates = fixture.candidates()?;
    assert!(candidates.required.contains(&fixture.skill));
    assert!(!candidates.required.contains(&fixture.optional));
    assert!(
        candidates
            .candidates
            .values()
            .all(|candidate| candidate.block_id != fixture.skill)
    );
    let input = crate::gateway::routing::input::Input::from_prompt(&fixture.prompt, 4096);
    let request = candidates
        .request(&fixture.store, "choice", &fixture.policy, &input)?
        .ok_or_else(|| invalid("bounded request was not prepared"))?;
    let blocks = request.state["blocks"]
        .as_object()
        .ok_or_else(|| invalid("request blocks missing"))?;
    assert!(!blocks.contains_key(&fixture.skill));
    assert!(blocks.contains_key(&fixture.optional));
    Ok(())
}

#[test]
fn compiler_keeps_skill_bytes_even_with_a_lossy_candidate_answer() -> Result<(), CoreError> {
    let mut fixture = Fixture::new()?;
    let summary = fixture.store.publish_summary(
        "task",
        vec![fixture.skill.clone()],
        "lossy skill summary".into(),
    )?;
    let extract = fixture.store.publish_extract(
        "task",
        &fixture.skill,
        vec![SourceSpan {
            message_index: 0,
            content_index: 0,
            start: 0,
            end: 5,
        }],
    )?;
    let mut candidates = fixture.candidates()?;
    // A stale or incorrectly constructed candidate set cannot bypass the
    // compiler's independent check of the immutable source binding.
    candidates.required.clear();
    candidates.candidates.insert(
        "skill".into(),
        Candidate {
            block_id: fixture.skill.clone(),
            summary_id: Some(summary),
            extract_id: Some(extract),
            excerpt: "bound skill evidence".into(),
        },
    );
    let before = digest(&fixture.store)?;
    for choice in ["hide", "summary", "extract"] {
        let response = DecisionResponse {
            model: "choice".into(),
            answers: candidates
                .candidates
                .iter()
                .map(|(id, candidate)| {
                    let choice = if candidate.block_id == fixture.skill {
                        choice
                    } else {
                        "hide"
                    };
                    (
                        id.clone(),
                        Answer::Choice {
                            choice: choice.into(),
                            confidence: 1.0,
                            probabilities: BTreeMap::from([(choice.into(), 1.0)]),
                        },
                    )
                })
                .collect(),
            usage: DecisionUsage {
                input_tokens: 1,
                output_tokens: 1,
            },
        };
        let (view, prompt) = candidates.compile(
            &fixture.store,
            &fixture.prompt,
            CompileOptions {
                source: fixture.source()?,
                policy: &fixture.policy,
                response: Some(&response),
                decision_id: Some("owned-choice".into()),
                reason: "typed_context_decision",
                hard_limit_bytes: 4096,
            },
        )?;
        assert_eq!(view.omitted, vec![fixture.optional.clone()]);
        assert_eq!(
            view.selected,
            vec![ContextRepresentation::Full {
                block_id: fixture.skill.clone()
            }]
        );
        assert_eq!(
            prompt.messages,
            fixture.store.evidence[&fixture.skill].messages
        );
        assert_eq!(prompt.system, fixture.prompt.system);
        assert_eq!(digest(&fixture.store)?, before);
    }
    Ok(())
}

#[test]
fn bounded_request_failure_retains_unjudged_evidence_and_enforces_limit() -> Result<(), CoreError> {
    let mut fixture = Fixture::new()?;
    fixture.policy.max_request_bytes = 1;
    let mut candidates = fixture.candidates()?;
    let input = crate::gateway::routing::input::Input::from_prompt(&fixture.prompt, 4096);
    assert!(
        candidates
            .request(&fixture.store, "choice", &fixture.policy, &input)?
            .is_none()
    );
    assert!(candidates.candidates.is_empty());
    let options = |hard_limit_bytes| -> Result<_, CoreError> {
        Ok(CompileOptions {
            source: fixture.source()?,
            policy: &fixture.policy,
            response: None,
            decision_id: None,
            reason: "conservative_context",
            hard_limit_bytes,
        })
    };
    let (view, prompt) = candidates.compile(&fixture.store, &fixture.prompt, options(4096)?)?;
    assert!(view.omitted.is_empty());
    assert_eq!(prompt, fixture.prompt);
    let error = candidates
        .compile(&fixture.store, &fixture.prompt, options(1)?)
        .err()
        .ok_or_else(|| invalid("oversized retained context was admitted"))?;
    assert_eq!(error.code, ErrorCode::NoFeasibleRoute);
    Ok(())
}

#[test]
fn captured_skill_message_keeps_binding_and_cannot_cross_tasks() -> Result<(), CoreError> {
    let mut fixture = Fixture::new()?;
    let messages = vec![Message::text(Role::User, "verified skill resource bytes")];
    let bindings = BTreeMap::from([(digest(&messages)?, "approved-skill-binding".into())]);
    let source = EvidenceSource::from_manifest("task", "agent", &fixture.manifest);
    let ids =
        fixture
            .store
            .capture_with_sources(&messages, &source, &BTreeMap::new(), &bindings)?;
    let id = ids.first().ok_or_else(|| invalid("skill block missing"))?;
    let block = fixture.store.evidence[id].clone();
    assert_eq!(
        block.source.skill_binding.as_deref(),
        Some("approved-skill-binding")
    );
    assert_eq!(block.source.permission_revision, source.permission_revision);
    assert_eq!(
        block.source.tool_manifest_digest,
        source.tool_manifest_digest
    );
    assert_eq!(block.messages, messages);
    assert!(block.protected);
    assert!(!crate::storage::checkpoint::snapshot::context::portable(
        &block
    ));
    assert!(crate::core::context_router::tasks::render(&block).is_err());
    let mut changed = source;
    changed.permission_revision += 1;
    let repeated =
        fixture
            .store
            .capture_with_sources(&messages, &changed, &BTreeMap::new(), &bindings)?;
    assert_eq!(repeated, ids);
    assert_eq!(fixture.store.evidence[id], block);
    Ok(())
}
