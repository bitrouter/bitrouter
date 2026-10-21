//! ACP delivery reads explicitly selected local skill directories.
//! The supervisor remains the sole owner of input identity and the dispatch journal.

use anyhow::{Result, ensure};
use async_trait::async_trait;
use bitrouter_orchestrator::acp::skills::{self, Capabilities, Instructions, Prepared, Target};
use bitrouter_orchestrator::core::protocol::{CoreError, ErrorCode};
use bitrouter_orchestrator::core::skills::{
    SkillCandidate, SkillConstraints, SkillSelector, SkillSource, selection,
};
use bitrouter_orchestrator::harness::skills::library::SkillDirectory;

use serde::{Deserialize, Serialize};

use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub session_id: String,
    pub turn_id: String,
    pub stage: String,
    pub capabilities: Capabilities,
    pub constraints: SkillConstraints,
    pub candidates: Vec<SkillCandidate>,
    pub problems: Vec<String>,

    pub receipt: Option<skills::Receipt>,
    pub failure: Option<String>,
}

#[async_trait]
pub(crate) trait Journal: Send + Sync {
    /// Acknowledgement is a dispatch barrier, not a best-effort telemetry write.
    async fn save(&self, report: &Report) -> Result<()>;
}

pub(crate) struct Context {
    pub constraints: SkillConstraints,
}

impl Context {
    pub async fn prepare(
        &self,
        target: Target<'_>,
        prompt: &str,
        capabilities: &Capabilities,
        cancel: &CancellationToken,
        journal: &dyn Journal,
    ) -> Result<Option<Prepared>> {
        self.constraints.validate()?;
        ensure!(
            !self
                .constraints
                .include
                .iter()
                .any(|selector| matches!(selector, SkillSelector::Mcp { .. })),
            "unsupported: external ACP cannot preserve MCP-origin manifest/consent/cache/execution boundaries"
        );
        ensure!(
            !cancel.is_cancelled(),
            "skill delivery cancelled before discovery"
        );
        if skills::is_native_command(prompt) {
            ensure!(
                self.constraints.include.is_empty(),
                "unsupported: required skill delivery cannot preserve native slash-command semantics"
            );
            return Ok(None);
        }
        if self.constraints.include.is_empty() {
            return Ok(None);
        }
        let mut report = Report {
            session_id: target.session_id.into(),
            turn_id: target.turn_id.into(),
            stage: "catalogued".into(),
            capabilities: capabilities.clone(),
            constraints: self.constraints.clone(),
            candidates: vec![],
            problems: vec![],

            receipt: None,
            failure: None,
        };
        if !capabilities.prompt_instructions {
            report.stage = "unsupported".into();
            report.problems.push(capabilities.reason.clone());
            journal.save(&report).await?;
            ensure!(
                self.constraints.include.is_empty(),
                "unsupported: {}",
                capabilities.reason
            );
            return Ok(None);
        }
        let library = super::native::Library::runtime();
        let catalog = tokio::select! {
            biased;
            ()=cancel.cancelled()=>anyhow::bail!("skill discovery cancelled"),
            result=library.catalog(target.workspace,target.caller)=>result.map_err(anyhow::Error::msg)?,
        };
        report.problems = catalog
            .problems
            .iter()
            .take(32)
            .map(|problem| truncate(problem, 2048))
            .collect();
        // Explicit identities reserve metadata capacity before unrelated entries.
        let mut sources: Vec<_> = catalog.skills.into_iter().map(SkillSource::Local).collect();
        sources.sort_by_key(|source| !self.constraints.include.contains(&source.selector()));
        let mut metadata_bytes = 0_usize;
        for source in sources.into_iter().take(256) {
            let bytes = serde_json::to_vec(&source)?.len();
            if bytes > 64 * 1024 || metadata_bytes.saturating_add(bytes) > 512 * 1024 {
                ensure!(
                    !self.constraints.include.contains(&source.selector()),
                    "required skill metadata exceeds the ACP instruction/evidence budget"
                );
                report.problems.push(format!(
                    "{:?}: candidate_metadata_budget",
                    source.selector()
                ));
                continue;
            }
            metadata_bytes += bytes;
            report.candidates.push(SkillCandidate {
                source,
                reason: String::new(),
                selected: false,
                content: None,
                files: Default::default(),
                failure: None,
                consents: vec![],
            });
        }
        journal.save(&report).await?;
        selection::select(&mut report.candidates, &self.constraints)?;
        selection::bound(&report.candidates, 4, 64 * 1024, |source| {
            skills::instruction_size(source, target).map_err(|error| {
                CoreError::rejected(ErrorCode::ArtifactUnavailable, error.to_string())
            })
        })?;
        report.stage = "selected".into();
        journal.save(&report).await?;
        let mut instructions = Vec::new();
        for candidate in &mut report.candidates {
            if !candidate.selected {
                continue;
            }
            ensure!(!cancel.is_cancelled(), "skill loading cancelled");
            let SkillSource::Local(binding) = &candidate.source else {
                anyhow::bail!("unsupported: MCP-origin ACP skill");
            };
            let bytes = tokio::select! {
                biased;
                ()=cancel.cancelled()=>anyhow::bail!("skill loading cancelled"),
                result=library.read(target.workspace,target.caller,binding,"SKILL.md",64*1024)=>result,
            };
            match bytes {
                Ok(bytes) => instructions.push(Instructions {
                    source: candidate.source.clone(),
                    bytes,
                }),
                Err(error) => {
                    candidate.selected = false;
                    candidate.reason = "instructions_unavailable".into();
                    candidate.failure = Some(CoreError::rejected(
                        ErrorCode::ArtifactUnavailable,
                        truncate(&error, 2048),
                    ));
                    if self
                        .constraints
                        .include
                        .contains(&candidate.source.selector())
                    {
                        report.stage = "failed".into();
                        report.failure = Some(truncate(&error, 2048));
                        journal.save(&report).await?;
                        anyhow::bail!(
                            "required skill instructions unavailable: {}",
                            truncate(&error, 2048)
                        );
                    }
                }
            }
        }
        ensure!(
            !cancel.is_cancelled(),
            "skill delivery cancelled before preparation"
        );
        if instructions.is_empty() {
            report.stage = "no_selection".into();
            journal.save(&report).await?;
            return Ok(None);
        }
        let prepared = skills::prepare(capabilities, target, prompt, instructions)?;
        report.receipt = Some(prepared.receipt.clone());
        report.stage = "prepared".into();
        journal.save(&report).await?;
        ensure!(
            !cancel.is_cancelled(),
            "skill delivery cancelled before dispatch"
        );
        Ok(Some(prepared))
    }
}

pub(crate) fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}
