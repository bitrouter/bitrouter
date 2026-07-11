//! ACP agent registry client — tier-1 integration with the official registry
//! (<https://agentclientprotocol.com/get-started/registry>).
//!
//! The registry is a curated JSON document listing ACP-compatible agents with
//! machine-readable `distribution` blocks. Tier 1 maps the package-runner
//! distributions (`npx`, `uvx`) onto BitRouter's stdio agent transport — the
//! runner downloads and executes the package, so there is no binary for us to
//! fetch or verify. `binary` distributions are surfaced in listings but not
//! auto-installable: the registry carries no checksums or signatures yet, so
//! installing them stays a manual, user-verified step.
//!
//! The registry is a discovery + install surface only. `bitrouter.yaml`'s
//! `agents:` block remains the source of truth for what can actually launch.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

/// The published registry document (CDN, updated by the ACP project).
pub const REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// How long a registry fetch may take before the CLI gives up. Discovery is a
/// convenience; a slow CDN must not hang `bitrouter agents`.
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The registry document root.
#[derive(Debug, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub agents: Vec<RegistryAgent>,
}

/// One agent entry. Unknown fields are ignored so registry additions never
/// break parsing.
#[derive(Debug, Deserialize)]
pub struct RegistryAgent {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub distribution: Option<Distribution>,
}

/// How an agent is distributed. Exactly the runners we can map onto stdio
/// (`npx`, `uvx`) are modeled; `binary` is kept opaque — its presence marks
/// the entry as manual-install.
#[derive(Debug, Deserialize)]
pub struct Distribution {
    #[serde(default)]
    pub npx: Option<PackageRun>,
    #[serde(default)]
    pub uvx: Option<PackageRun>,
    #[serde(default)]
    pub binary: Option<serde_json::Value>,
}

/// A package-runner invocation (`npx` / `uvx`).
#[derive(Debug, Deserialize)]
pub struct PackageRun {
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Env the agent needs at runtime (e.g. an API-key variable name mapped
    /// to a placeholder). `BTreeMap` keeps stub output deterministic.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A concrete stdio invocation derived from a registry distribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StdioInvocation {
    pub command: &'static str,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// How an entry can be installed, for listings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallSupport {
    /// `npx`/`uvx` — `bitrouter agents install` emits a ready stub.
    Stub(&'static str),
    /// `binary`-only — must be installed manually (no checksums to verify).
    Manual,
    /// No distribution block at all.
    None,
}

impl RegistryAgent {
    /// The stdio invocation for this agent, when its distribution uses a
    /// package runner we can invoke directly. Preference order: `npx`, then
    /// `uvx`. `binary`-only (or missing) distributions return `None`.
    pub fn stdio_invocation(&self) -> Option<StdioInvocation> {
        let dist = self.distribution.as_ref()?;
        if let Some(npx) = &dist.npx {
            let mut args = vec!["-y".to_string(), npx.package.clone()];
            args.extend(npx.args.iter().cloned());
            return Some(StdioInvocation {
                command: "npx",
                args,
                env: npx.env.clone(),
            });
        }
        if let Some(uvx) = &dist.uvx {
            let mut args = vec![uvx.package.clone()];
            args.extend(uvx.args.iter().cloned());
            return Some(StdioInvocation {
                command: "uvx",
                args,
                env: uvx.env.clone(),
            });
        }
        None
    }

    /// Install support classification for listings.
    pub fn install_support(&self) -> InstallSupport {
        match &self.distribution {
            None => InstallSupport::None,
            Some(d) if d.npx.is_some() => InstallSupport::Stub("npx"),
            Some(d) if d.uvx.is_some() => InstallSupport::Stub("uvx"),
            Some(d) if d.binary.is_some() => InstallSupport::Manual,
            Some(_) => InstallSupport::None,
        }
    }
}

/// Parse a registry document from its JSON text.
pub fn parse(json: &str) -> Result<Registry> {
    serde_json::from_str(json).context("parsing ACP agent registry JSON")
}

/// Fetch and parse the registry from `url` (callers pass [`REGISTRY_URL`];
/// tests use [`parse`] on a fixture instead of the network).
pub async fn fetch(url: &str) -> Result<Registry> {
    let client = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .build()
        .context("building registry http client")?;
    let body = client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .with_context(|| format!("fetching ACP agent registry from {url}"))?
        .text()
        .await
        .context("reading ACP agent registry body")?;
    parse(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trimmed real-shape fixture: one npx entry (with args), one uvx entry
    /// (with env), one binary-only entry, one with no distribution.
    const FIXTURE: &str = r#"{
      "version": "1",
      "agents": [
        {
          "id": "gemini",
          "name": "Gemini CLI",
          "version": "0.50.0",
          "description": "Google's official CLI for Gemini",
          "repository": "https://github.com/google-gemini/gemini-cli",
          "license": "Apache-2.0",
          "distribution": { "npx": { "package": "@google/gemini-cli@0.50.0", "args": ["--acp"] } }
        },
        {
          "id": "pytool",
          "name": "PyTool",
          "distribution": { "uvx": { "package": "pytool-acp@1.2.0", "env": { "PYTOOL_API_KEY": "" } } }
        },
        {
          "id": "opencode",
          "name": "OpenCode",
          "version": "1.17.15",
          "repository": "https://github.com/anomalyco/opencode",
          "distribution": {
            "binary": {
              "darwin-aarch64": { "archive": "https://example.com/opencode.zip", "cmd": "./opencode", "args": ["acp"] }
            }
          }
        },
        { "id": "mystery", "name": "Mystery", "unknown_future_field": 42 }
      ]
    }"#;

    #[test]
    fn parses_real_shape_and_ignores_unknown_fields() {
        let reg = parse(FIXTURE).expect("fixture parses");
        assert_eq!(reg.agents.len(), 4);
        assert_eq!(reg.agents[0].id, "gemini");
        assert_eq!(reg.agents[0].version.as_deref(), Some("0.50.0"));
    }

    #[test]
    fn npx_maps_to_stdio_invocation_with_dash_y() {
        let reg = parse(FIXTURE).expect("parse");
        let inv = reg.agents[0].stdio_invocation().expect("npx maps");
        assert_eq!(inv.command, "npx");
        assert_eq!(inv.args, vec!["-y", "@google/gemini-cli@0.50.0", "--acp"]);
        assert!(inv.env.is_empty());
    }

    #[test]
    fn uvx_maps_to_stdio_invocation_with_env() {
        let reg = parse(FIXTURE).expect("parse");
        let inv = reg.agents[1].stdio_invocation().expect("uvx maps");
        assert_eq!(inv.command, "uvx");
        assert_eq!(inv.args, vec!["pytool-acp@1.2.0"]);
        assert!(inv.env.contains_key("PYTOOL_API_KEY"));
    }

    #[test]
    fn binary_only_is_manual_and_has_no_invocation() {
        let reg = parse(FIXTURE).expect("parse");
        assert_eq!(reg.agents[2].stdio_invocation(), None);
        assert_eq!(reg.agents[2].install_support(), InstallSupport::Manual);
    }

    #[test]
    fn missing_distribution_classifies_as_none() {
        let reg = parse(FIXTURE).expect("parse");
        assert_eq!(reg.agents[3].install_support(), InstallSupport::None);
        assert_eq!(reg.agents[3].stdio_invocation(), None);
    }
}
