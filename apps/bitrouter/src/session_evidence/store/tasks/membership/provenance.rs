//! Recheck the original controller/process attachment without reopening any
//! message-supplied path or relying on a live controller instance.

use std::path::{Component, Path, PathBuf};

use super::*;
use crate::session_evidence::native_inputs::NativeInputBinding;

impl EvidenceStore {
    pub(super) async fn input_attachment_on(
        &self,
        db: &impl ConnectionTrait,
        input: &NativeInputBinding,
        evidence: &AttemptExecutions,
    ) -> Result<()> {
        let (controller, registration) = self
            .membership_record(db, &input.controller_registration)
            .await?;
        ensure!(
            input.controller_registration.range.start == 0
                && input.controller_registration.range.source_id
                    == input.origin.request.range.source_id
                && controller.format == SourceFormat::Acp
                && controller.node.is_none()
                && controller.namespace == input.origin.session.namespace
                && controller.harness == input.node.harness
                && controller.locator == format!("controller:{}", input.origin.controller_id),
            "input controller registration mismatch"
        );
        let payload = &registration.input.raw["payload"];
        ensure!(
            registration.input.raw["phase"] == "metadata"
                && matches!(
                    registration.input.raw["method"].as_str(),
                    Some("controller/started" | "controller/root_registered")
                )
                && payload["namespace"].as_str() == Some(&controller.namespace)
                && payload["harness"] == serde_json::to_value(controller.harness)?,
            "input root registration invalid"
        );
        let root = absolute_path(&payload["native_root"])?;
        let spool = absolute_path(&payload["spool"])?;
        ensure!(
            canonical_digest(&(controller.harness, &root))? == controller.namespace,
            "input native root identity mismatch"
        );
        let (source, header) = self.membership_record(db, &input.process_header).await?;
        ensure!(
            input.process_header.range.source_id == input.input.range.source_id
                && input.process_header.range.start == 0
                && input.process_header.range.generation == "spool/1"
                && source.node.is_none()
                && source.namespace == controller.namespace
                && source.harness == controller.harness
                && header.input.raw["phase"] == "metadata"
                && header.input.raw["method"] == "runtime/started",
            "input process header mismatch"
        );
        let path = Path::new(
            source
                .locator
                .strip_prefix("spool:")
                .context("input process locator missing")?,
        );
        let filename = match source.harness {
            Harness::Codex => {
                ensure!(
                    source.format == SourceFormat::CodexAppServer
                        && input.configuration.is_none()
                        && input.session_response.is_none(),
                    "invalid Codex process binding"
                );
                format!("{}.jsonl", input.process_id)
            }
            Harness::ClaudeCode => {
                ensure!(
                    source.format == SourceFormat::ClaudeCli
                        && header.input.raw["process_id"].as_str() == Some(&input.process_id)
                        && header.input.raw["namespace"].as_str() == Some(&controller.namespace)
                        && header.input.raw["scope_valid"] == true,
                    "Claude process identity mismatch"
                );
                self.claude_attachment_on(db, input, payload, &header.input.raw, evidence)
                    .await?;
                format!("cli-{}.jsonl", input.process_id)
            }
        };
        ensure!(
            uuid::Uuid::parse_str(&input.process_id)?.to_string() == input.process_id
                && path.parent() == Some(spool.as_path())
                && path.file_name().and_then(|name| name.to_str()) == Some(filename.as_str()),
            "input process belongs to another controller or profile"
        );
        if let Some(history) = &input.codex_history {
            ensure!(
                input.node.harness == Harness::Codex,
                "foreign input rollout"
            );
            if let Some(lifecycle) = &history.lifecycle {
                ensure!(
                    lifecycle.thread_id == input.node.native_id,
                    "input rollout lifecycle thread mismatch"
                );
                if let Some(identity) = &history.source {
                    let observed = Path::new(&lifecycle.path);
                    ensure!(
                        observed.starts_with(&root)
                            && observed
                                .strip_prefix(&root)?
                                .components()
                                .all(|part| matches!(part, Component::Normal(_))),
                        "input rollout outside original native root"
                    );
                    ensure!(
                        identity.node == input.node
                            && identity.rollout_id == lifecycle.rollout_id
                            && identity.metadata.range.source_id == identity.id
                            && identity.metadata.range.start == 0,
                        "input selected rollout identity mismatch"
                    );
                    let (source, metadata) = self.membership_record(db, &identity.metadata).await?;
                    ensure!(
                        source.format == SourceFormat::CodexRollout
                            && source.node.as_ref() == Some(&input.node)
                            && metadata.input.raw["type"] == "session_meta"
                            && metadata.input.raw["payload"]["id"].as_str()
                                == Some(&input.node.native_id),
                        "input selected rollout metadata mismatch"
                    );
                    let stored: crate::session_evidence::store::rollouts::RolloutIdentity =
                        decode_object(
                            self.object(db, "rollout_identity", &identity.id)
                                .await?
                                .context("selected rollout identity missing")?,
                        )?;
                    ensure!(stored == *identity, "selected rollout binding changed");
                }
            }
        }
        Ok(())
    }

    async fn claude_attachment_on(
        &self,
        db: &impl ConnectionTrait,
        input: &NativeInputBinding,
        registration: &Value,
        header: &Value,
        evidence: &AttemptExecutions,
    ) -> Result<()> {
        // The ACP attachment and native conversation can diverge after reset.
        // Match the original configuration and lifecycle operation, not the
        // command's native session_id to the public ACP session id.
        // https://code.claude.com/docs/en/agent-sdk/sessions
        // https://agentclientprotocol.com/protocol/v1/session-setup
        let config = input
            .configuration
            .as_ref()
            .context("Claude configuration missing")?;
        let response = input
            .session_response
            .as_ref()
            .context("Claude lifecycle response missing")?;
        ensure!(
            header["configuration_status"] == "present"
                && header["configured_by"] == serde_json::to_value(&config.configuration)?
                && config.controller_id == input.origin.controller_id,
            "Claude configuration header mismatch"
        );
        let (source, record) = self.membership_record(db, &config.configuration).await?;
        ensure!(
            source.locator == format!("controller:{}", config.controller_id)
                && source.namespace == input.node.namespace
                && source.harness == Harness::ClaudeCode
                && source.format == SourceFormat::Acp
                && source.node.is_none(),
            "Claude configuration source mismatch"
        );
        let payload = &record.input.raw["payload"];
        ensure!(
            record.input.raw["phase"] == "metadata"
                && record.input.raw["method"] == "controller/process_configured"
                && record.input.raw["operation_id"].as_str() == Some(&config.operation_id)
                && payload["method"].as_str() == Some(&config.method)
                && payload["request"] == serde_json::to_value(&config.request)?,
            "Claude configuration operation mismatch"
        );
        for field in ["namespace", "native_root", "spool"] {
            ensure!(
                payload.get(field) == registration.get(field),
                "Claude configuration registration mismatch"
            );
        }
        let cwd = PathBuf::from(
            payload["cwd"]
                .as_str()
                .context("Claude configuration cwd missing")?,
        );
        ensure!(
            cwd.is_absolute()
                && cwd == config.cwd
                && optional_id(payload, "sessionId")? == config.requested_session_id
                && matches!(
                    config.method.as_str(),
                    "session/new" | "session/load" | "session/resume" | "session/fork"
                ),
            "Claude configuration parameters mismatch"
        );
        let (request_source, request) = self.membership_record(db, &config.request).await?;
        let (response_source, returned) = self.membership_record(db, &response.record).await?;
        for (source, reference, record, phase) in [
            (&request_source, &config.request, &request, "request"),
            (&response_source, &response.record, &returned, "response"),
        ] {
            ensure!(
                source.format == SourceFormat::Acp
                    && source.node.is_none()
                    && source.harness == Harness::ClaudeCode
                    && source.locator == format!("controller:{}", config.controller_id)
                    && record.input.raw["phase"] == phase
                    && record.input.raw["method"].as_str() == Some(&config.method)
                    && record.input.raw["operation_id"].as_str() == Some(&config.operation_id),
                "Claude lifecycle belongs to another controller operation"
            );
            ensure!(
                evidence
                    .prefixes
                    .iter()
                    .any(|prefix| prefix.range.source_id == reference.range.source_id
                        && prefix.range.generation == "controller/1"
                        && prefix.range.start == 0),
                "Claude lifecycle registration outside inspected evidence"
            );
        }
        ensure!(
            config.request.range.source_id != config.configuration.range.source_id
                || config.request.range.start < config.configuration.range.start,
            "Claude configuration precedes request"
        );
        ensure!(
            config.request.range.source_id != response.record.range.source_id
                || config.request.range.start < response.record.range.start,
            "Claude response precedes request"
        );
        ensure!(
            request.input.raw["payload"].get("cwd") == payload.get("cwd")
                && optional_id(&request.input.raw["payload"], "sessionId")?
                    == config.requested_session_id,
            "Claude lifecycle request parameters changed"
        );
        let returned = &returned.input.raw["payload"];
        ensure!(
            returned.get("error_code").is_none() && response.error_code.is_none(),
            "Claude lifecycle failed"
        );
        let session = optional_id(returned, "sessionId")?.or_else(|| {
            matches!(config.method.as_str(), "session/load" | "session/resume")
                .then(|| config.requested_session_id.clone())
                .flatten()
        });
        ensure!(
            session.as_ref() == Some(&input.origin.session.session_id)
                && response.acp_session_id == session,
            "Claude process ACP attachment mismatch"
        );
        Ok(())
    }

    pub(super) async fn membership_record(
        &self,
        db: &impl ConnectionTrait,
        reference: &RecordRef,
    ) -> Result<(SourceDescriptor, StoredRecord)> {
        reference.validate()?;
        let source = source_entity::Entity::find_by_id(&reference.range.source_id)
            .filter(source_entity::Column::Owner.eq(&self.owner_key))
            .one(db)
            .await?
            .context("membership source missing")?;
        let source = decode_source(source)?;
        let record = range_records(db, &self.owner_key, &reference.range)
            .await?
            .into_iter()
            .next()
            .context("membership record missing")?;
        ensure!(
            RecordRef::from_record(&record)? == *reference,
            "membership reference mismatch"
        );
        Ok((source.descriptor, record))
    }
}

fn absolute_path(value: &Value) -> Result<PathBuf> {
    let path = PathBuf::from(value.as_str().context("membership path missing")?);
    ensure!(
        path.is_absolute()
            && !path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir)),
        "membership path not normalized"
    );
    Ok(path)
}

fn optional_id(payload: &Value, field: &str) -> Result<Option<String>> {
    payload
        .get(field)
        .filter(|value| !value.is_null())
        .map(|value| {
            let id = value.as_str().context("membership optional id invalid")?;
            identifier(id)?;
            Ok(id.to_owned())
        })
        .transpose()
}
