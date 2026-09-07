//! Verify the configuration consumed by a native process against owned records.
//! A saved configuration can start several processes. It does not establish
//! the cause of a restart, Query liveness, task membership or task completion.

use super::*;
use crate::session_evidence::execution::{FactKind, extract};
use crate::session_evidence::types::{RegisteredSource, StoredRecord, identifier};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessBinding {
    pub source_id: String,
    pub process_id: Option<String>,
    pub header: Option<RecordRef>,
    pub configured_by: Option<ProcessConfiguration>,
    pub gaps: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessConfiguration {
    pub controller_id: String,
    pub operation_id: String,
    pub method: String,
    pub requested_session_id: Option<String>,
    pub cwd: PathBuf,
    pub configuration: RecordRef,
    pub request: RecordRef,
}

impl ControllerEvidence {
    pub(super) async fn remember_process_source(&self, id: &str, gaps: &mut BTreeSet<String>) {
        let mut state = self.state.lock().await;
        if state.process_sources.contains(id) || state.process_sources.len() < MAX_GRAPH_ITEMS {
            state.process_sources.insert(id.into());
        } else {
            gaps.insert("native_process_source_limit".into());
        }
    }

    pub(super) async fn process_binding(&self, source_id: String) -> ProcessBinding {
        let mut binding = ProcessBinding {
            source_id,
            process_id: None,
            header: None,
            configured_by: None,
            gaps: BTreeSet::new(),
        };
        let header = self.process_header(&binding.source_id).await;
        let (source, record, process_id) = match header {
            Ok(header) => header,
            Err(error) => {
                tracing::warn!(%error, "native process header could not be verified");
                binding.gaps.insert("native_process_header_invalid".into());
                return binding;
            }
        };
        binding.process_id = Some(process_id);
        match RecordRef::from_record(&record) {
            Ok(reference) => binding.header = Some(reference),
            Err(_) => {
                binding.gaps.insert("native_process_header_invalid".into());
                return binding;
            }
        }
        let raw = &record.input.raw;
        if raw.get("configured_by").is_none_or(Value::is_null)
            && raw.get("configuration_status").and_then(Value::as_str) != Some("invalid")
        {
            binding
                .gaps
                .insert("native_process_configuration_missing".into());
            return binding;
        }
        match self.process_configuration(&source, raw).await {
            Ok(configuration) => binding.configured_by = Some(configuration),
            Err(error) => {
                tracing::warn!(%error, "native process configuration could not be verified");
                binding
                    .gaps
                    .insert("native_process_configuration_invalid".into());
            }
        }
        binding
    }

    async fn process_header(&self, id: &str) -> Result<(RegisteredSource, StoredRecord, String)> {
        let source = self
            .store
            .source(id)
            .await?
            .context("process source missing")?;
        ensure!(
            source.descriptor.format == SourceFormat::ClaudeCli
                && source.descriptor.harness == Harness::ClaudeCode
                && source.descriptor.node.is_none()
                && source.cursor.generation == "spool/1"
                && source.cursor.next_sequence > 0,
            "invalid process source"
        );
        let record = self
            .store
            .records(&SourceRange {
                source_id: source.id.clone(),
                generation: "spool/1".into(),
                start: 0,
                end: 1,
            })
            .await?
            .into_iter()
            .next()
            .context("process header missing")?;
        ensure!(
            record.input.raw.get("phase").and_then(Value::as_str) == Some("metadata"),
            "process header is not metadata"
        );
        let facts = extract(&source.descriptor, &record)?;
        ensure!(facts.len() == 1, "process header fact missing");
        let fact = facts.first().context("process fact missing")?;
        ensure!(
            matches!(&fact.event, FactKind::ProcessLifecycle { state, .. }
            if matches!(state.as_str(), "started" | "failed")),
            "invalid process header fact"
        );
        let process_id = fact
            .process_id
            .clone()
            .context("process identity missing")?;
        Ok((source, record, process_id))
    }

    async fn process_configuration(
        &self,
        process: &RegisteredSource,
        raw: &Value,
    ) -> Result<ProcessConfiguration> {
        ensure!(
            raw.get("configuration_status").and_then(Value::as_str) == Some("present"),
            "process configuration reference unavailable"
        );
        let configuration: RecordRef = serde_json::from_value(
            raw.get("configured_by")
                .context("configuration reference missing")?
                .clone(),
        )?;
        let (source, record) = referenced_record(&self.store, &configuration).await?;
        let root = self.recovered_root(&source).await?;
        let process_path = Path::new(
            process
                .descriptor
                .locator
                .strip_prefix("spool:")
                .context("process spool missing")?,
        );
        ensure!(
            process.descriptor.namespace == source.descriptor.namespace
                && process_path.parent() == Some(root.spool.as_path()),
            "process configuration belongs to another profile or controller"
        );
        let raw = &record.input.raw;
        ensure!(
            raw.get("method").and_then(Value::as_str) == Some("controller/process_configured")
                && raw.get("phase").and_then(Value::as_str) == Some("metadata"),
            "reference is not process configuration"
        );
        let payload = raw
            .get("payload")
            .context("configuration payload missing")?;
        let native = root.collector.root();
        ensure!(
            payload.get("namespace") == Some(&json!(native.namespace))
                && payload.get("native_root") == Some(&json!(native.directory))
                && payload.get("spool") == Some(&json!(root.spool)),
            "configuration registration mismatch"
        );
        let method = field(payload, "method")?;
        ensure!(
            matches!(
                method.as_str(),
                "session/new" | "session/load" | "session/resume" | "session/fork"
            ),
            "configuration is not a session lifecycle operation"
        );
        let operation_id = field(raw, "operation_id")?;
        let requested_session_id = optional_id(payload, "sessionId")?;
        let cwd = PathBuf::from(
            payload
                .get("cwd")
                .and_then(Value::as_str)
                .context("configuration cwd missing")?,
        );
        ensure!(cwd.is_absolute(), "configuration cwd is not absolute");
        let request: RecordRef = serde_json::from_value(
            payload
                .get("request")
                .context("original configuration request missing")?
                .clone(),
        )?;
        let (request_source, request_record) = referenced_record(&self.store, &request).await?;
        // The request can precede selection of a different native profile. Its
        // controller identity must still match, and both root registrations
        // must belong to this application's evidence home.
        self.recovered_root(&request_source).await?;
        ensure!(
            request_source.descriptor.locator == source.descriptor.locator
                && request_source.descriptor.harness == source.descriptor.harness,
            "configuration request belongs to another controller"
        );
        ensure!(
            request.range.source_id != configuration.range.source_id
                || request.range.start < configuration.range.start,
            "configuration precedes its original request"
        );
        let observed = &request_record.input.raw;
        ensure!(
            observed.get("phase").and_then(Value::as_str) == Some("request")
                && observed.get("method").and_then(Value::as_str) == Some(method.as_str())
                && observed.get("operation_id").and_then(Value::as_str)
                    == Some(operation_id.as_str()),
            "configuration request operation mismatch"
        );
        let observed = observed
            .get("payload")
            .context("original request payload missing")?;
        ensure!(
            observed.get("cwd") == payload.get("cwd")
                && optional_id(observed, "sessionId")? == requested_session_id,
            "configuration request parameters mismatch"
        );
        Ok(ProcessConfiguration {
            controller_id: source
                .descriptor
                .locator
                .strip_prefix("controller:")
                .context("configuration controller missing")?
                .into(),
            operation_id,
            method,
            requested_session_id,
            cwd,
            configuration,
            request,
        })
    }
}

async fn referenced_record(
    store: &EvidenceStore,
    reference: &RecordRef,
) -> Result<(RegisteredSource, StoredRecord)> {
    reference.validate()?;
    let source = store
        .source(&reference.range.source_id)
        .await?
        .context("referenced source missing")?;
    let record = store
        .records(&reference.range)
        .await?
        .into_iter()
        .next()
        .context("referenced record missing")?;
    ensure!(
        RecordRef::from_record(&record)? == *reference,
        "referenced record changed"
    );
    Ok((source, record))
}

fn field(value: &Value, key: &str) -> Result<String> {
    optional_id(value, key)?.context("process configuration field missing")
}

fn optional_id(value: &Value, key: &str) -> Result<Option<String>> {
    value
        .get(key)
        .filter(|value| !value.is_null())
        .map(|value| {
            let text = value
                .as_str()
                .context("process configuration field is not text")?;
            identifier(text)?;
            Ok(text.into())
        })
        .transpose()
}

#[cfg(test)]
mod tests;
