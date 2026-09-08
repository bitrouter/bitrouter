//! Pinned producer observations linking an ACP prompt to native identifiers.
//! Enqueue/acceptance observations still require native execution corroboration.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::types::{AcpSessionKey, Harness, RecordRef, identifier};

pub const METHOD: &str = "_bitrouter/nativeBinding";
pub const META_KEY: &str = "bitrouter/native-evidence";
pub const MAX_OBSERVATIONS: u32 = 128;
const PINS: &str = include_str!("adapter_bridge/pins.json");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PromptOrigin {
    pub controller_id: String,
    pub operation_id: String,
    pub session: AcpSessionKey,
    pub request: RecordRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdapterIdentity {
    pub package: String,
    pub version: String,
    pub module_digest: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capability {
    schema: u32,
    adapter: AdapterIdentity,
}

pub fn capability(value: &Value, harness: Harness) -> Option<Value> {
    let capability: Capability = serde_json::from_value(value.clone()).ok()?;
    (capability.schema == 1 && capability.adapter == pin(harness).ok()?.identity())
        .then(|| serde_json::to_value(capability).ok())
        .flatten()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Event {
    Started,
    ClaudeEnqueued {
        command_id: String,
    },
    CodexAccepted {
        thread_id: String,
        turn_id: String,
        role: String,
    },
    /// Goal commands can infer this callback from later events. This is not
    /// equivalent to the direct turn/start response behind CodexAccepted.
    CodexCommandObserved {
        thread_id: Option<String>,
        turn_id: String,
    },
    Finished {
        outcome: String,
        notification_failures: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Observation {
    pub schema: u32,
    pub session_id: String,
    pub origin: PromptOrigin,
    pub adapter: AdapterIdentity,
    pub sequence: u32,
    pub event: Event,
}

impl Observation {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema == 1 && self.sequence < MAX_OBSERVATIONS,
            "unsupported adapter observation"
        );
        self.origin.session.validate()?;
        self.origin.request.validate()?;
        identifier(&self.origin.controller_id)?;
        identifier(&self.origin.operation_id)?;
        ensure!(
            self.session_id == self.origin.session.session_id,
            "adapter observation conversation mismatch"
        );
        let pin = pin(self.origin.session.harness)?;
        ensure!(
            self.adapter == pin.identity(),
            "unsupported adapter producer module"
        );
        match &self.event {
            Event::Started => ensure!(self.sequence == 0, "adapter start sequence mismatch"),
            Event::ClaudeEnqueued { command_id } => {
                ensure!(
                    self.origin.session.harness == Harness::ClaudeCode,
                    "unexpected Claude observation"
                );
                identifier(command_id)?;
            }
            Event::CodexAccepted {
                thread_id,
                turn_id,
                role,
            } => {
                ensure!(
                    self.origin.session.harness == Harness::Codex
                        && matches!(role.as_str(), "prompt" | "implementation"),
                    "unsupported Codex acceptance role"
                );
                identifier(thread_id)?;
                identifier(turn_id)?;
            }
            Event::CodexCommandObserved { thread_id, turn_id } => {
                ensure!(
                    self.origin.session.harness == Harness::Codex,
                    "unexpected Codex command observation"
                );
                if let Some(thread) = thread_id {
                    identifier(thread)?;
                }
                identifier(turn_id)?;
            }
            Event::Finished { outcome, .. } => ensure!(
                matches!(outcome.as_str(), "returned" | "threw"),
                "unsupported prompt outcome"
            ),
        }
        ensure!(
            matches!(self.event, Event::Started) || self.sequence > 0,
            "adapter observation has no start position"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenObservation {
    pub record: RecordRef,
    pub observation: Observation,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PromptEvidence {
    pub observations: Vec<ProvenObservation>,
    /// Integrity/transport gaps in the selected producer observations. Empty
    /// does not certify coverage of all native work (including auxiliary jobs).
    pub gaps: BTreeSet<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Pin {
    package: String,
    version: String,
    module_digest: String,
    bin: String,
    module: String,
}

impl Pin {
    fn identity(&self) -> AdapterIdentity {
        AdapterIdentity {
            package: self.package.clone(),
            version: self.version.clone(),
            module_digest: self.module_digest.clone(),
        }
    }
}

fn harness_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Codex => "codex",
        Harness::ClaudeCode => "claude",
    }
}

fn pin(harness: Harness) -> Result<Pin> {
    let mut pins: BTreeMap<String, Pin> = serde_json::from_str(PINS)?;
    pins.remove(harness_name(harness))
        .context("native adapter pin missing")
}

/// Keep only the bounded, typed metadata contract. Malformed notifications
/// remain visible as invalid evidence without importing arbitrary SDK fields.
pub fn notification_fields(params: &Value) -> Value {
    serde_json::from_value::<Observation>(params.clone())
        .ok()
        .filter(|observation| observation.validate().is_ok())
        .and_then(|observation| serde_json::to_value(observation).ok())
        .unwrap_or_else(|| json!({"invalid": true}))
}

pub async fn prepare_launch(
    directory: &Path,
    harness: Harness,
    command: &str,
    args: &[String],
) -> Result<Option<Vec<String>>> {
    let pin = pin(harness)?;
    let executable = Path::new(command)
        .file_name()
        .and_then(|name| name.to_str());
    if !matches!(executable, Some("npx" | "npx.cmd"))
        || !matches!(args.first().map(String::as_str), Some("-y" | "--yes"))
        || args.get(1) != Some(&format!("{}@{}", pin.package, pin.version))
    {
        return Ok(None);
    }
    ensure!(
        !pin.bin.is_empty() && !pin.module.is_empty(),
        "invalid native adapter pin"
    );
    let directory = directory.join("adapter-bridge");
    tokio::fs::create_dir_all(&directory).await?;
    for (name, contents) in [
        ("entry.mjs", include_str!("adapter_bridge/entry.mjs")),
        (
            "transform.mjs",
            include_str!("adapter_bridge/transform.mjs"),
        ),
        ("runtime.mjs", include_str!("adapter_bridge/runtime.mjs")),
        ("pins.json", PINS),
    ] {
        tokio::fs::write(directory.join(name), contents).await?;
    }
    let entry: PathBuf = tokio::fs::canonicalize(directory.join("entry.mjs")).await?;
    // npm resolves the same exact package and its private executable PATH.
    // Only the selected Node entry installs hooks; no inherited NODE_OPTIONS.
    // https://docs.npmjs.com/cli/v11/commands/npm-exec
    let mut prepared = vec![
        args[0].clone(),
        format!("--package={}@{}", pin.package, pin.version),
        "node".into(),
        entry
            .to_str()
            .context("adapter bridge path must be UTF-8")?
            .into(),
        harness_name(harness).into(),
        "--".into(),
    ];
    prepared.extend_from_slice(&args[2..]);
    Ok(Some(prepared))
}

#[cfg(test)]
mod tests;
