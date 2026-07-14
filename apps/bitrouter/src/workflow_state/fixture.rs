use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use bitrouter_sdk::language_model::protocol::InboundAdapter;
use bitrouter_sdk::language_model::protocol::chat_completions::ChatCompletionsAdapter;
use bitrouter_sdk::language_model::protocol::messages::MessagesAdapter;
use bitrouter_sdk::language_model::protocol::responses::ResponsesAdapter;
use bitrouter_sdk::language_model::types::Prompt;
use bitrouter_sdk::{BitrouterError, HeaderMap, Result};
use http::{HeaderName, HeaderValue};

use crate::policy_table_router::PolicyTable;
use crate::workflow_state::ir::{HarnessId, ProtocolKind, WorkflowStateKind};

#[derive(Debug)]
pub struct WorkflowTraceFixture {
    pub id: String,
    pub harness: HarnessId,
    pub protocol: ProtocolKind,
    pub headers: HeaderMap,
    pub raw_body: serde_json::Value,
    pub prompt: Prompt,
    pub expected: ExpectedWorkflowState,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExpectedWorkflowState {
    pub state_kind: WorkflowStateKind,
    pub baseline_fingerprint: String,
    pub confidence_min: f32,
}

#[derive(Debug, Deserialize)]
struct WireFixture {
    id: String,
    harness: HarnessId,
    protocol: ProtocolKind,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
    raw_body: serde_json::Value,
    #[serde(default)]
    canonical_prompt: Option<serde_json::Value>,
    expected: ExpectedWorkflowState,
}

impl WorkflowTraceFixture {
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self> {
        let text = fs::read_to_string(path.as_ref()).map_err(|e| {
            BitrouterError::internal(format!(
                "workflow fixture read {}: {e}",
                path.as_ref().display()
            ))
        })?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
            BitrouterError::bad_request(format!(
                "workflow fixture parse {}: {e}",
                path.as_ref().display()
            ))
        })?;
        Self::from_value(value)
    }

    pub fn from_value(value: serde_json::Value) -> Result<Self> {
        let wire: WireFixture = serde_json::from_value(value).map_err(|e| {
            BitrouterError::bad_request(format!("workflow fixture parse from value: {e}"))
        })?;
        let headers = parse_headers(wire.headers)?;
        let prompt = parse_prompt(
            &wire.protocol,
            wire.raw_body.clone(),
            wire.canonical_prompt.clone(),
        )?;
        Ok(Self {
            id: wire.id,
            harness: wire.harness,
            protocol: wire.protocol,
            headers,
            raw_body: wire.raw_body,
            prompt,
            expected: wire.expected,
        })
    }

    pub fn load_dir(path: impl AsRef<Path>) -> Result<Vec<Self>> {
        let mut files: Vec<PathBuf> = fs::read_dir(path.as_ref())
            .map_err(|e| {
                BitrouterError::internal(format!(
                    "workflow fixture dir read {}: {e}",
                    path.as_ref().display()
                ))
            })?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        files.into_iter().map(Self::load_file).collect()
    }

    pub fn load_tree(path: impl AsRef<Path>) -> Result<Vec<Self>> {
        let mut files = Vec::new();
        collect_json_files(path.as_ref(), &mut files)?;
        files.sort();
        files.into_iter().map(Self::load_file).collect()
    }

    pub fn baseline_fingerprint(&self) -> String {
        PolicyTable::fingerprint(&self.prompt)
    }
}

fn collect_json_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| {
        BitrouterError::internal(format!("workflow fixture tree read {}: {e}", dir.display()))
    })? {
        let path = entry
            .map_err(|e| BitrouterError::internal(format!("workflow fixture dir entry: {e}")))?
            .path();
        if path.is_dir() {
            collect_json_files(&path, files)?;
        } else if path.extension().is_some_and(|ext| ext == "json") {
            files.push(path);
        }
    }
    Ok(())
}

fn parse_headers(raw: std::collections::BTreeMap<String, String>) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for (name, value) in raw {
        let name: HeaderName = name.parse().map_err(|e| {
            BitrouterError::bad_request(format!("workflow fixture invalid header name: {e}"))
        })?;
        let value: HeaderValue = value.parse().map_err(|e| {
            BitrouterError::bad_request(format!("workflow fixture invalid header value: {e}"))
        })?;
        headers.insert(name, value);
    }
    Ok(headers)
}

pub(crate) fn parse_prompt(
    protocol: &ProtocolKind,
    body: serde_json::Value,
    canonical_prompt: Option<serde_json::Value>,
) -> Result<Prompt> {
    match protocol {
        ProtocolKind::ChatCompletions => ChatCompletionsAdapter.parse_request(body),
        ProtocolKind::Messages => MessagesAdapter.parse_request(body),
        ProtocolKind::Responses => ResponsesAdapter.parse_request(body),
        ProtocolKind::OpenClawRuntime | ProtocolKind::Unknown => canonical_prompt.map_or_else(
            || {
                Err(BitrouterError::bad_request(
                    "workflow fixture protocol cannot be parsed into a canonical Prompt yet",
                ))
            },
            |prompt_body| ChatCompletionsAdapter.parse_request(prompt_body),
        ),
    }
}
