//! Emit a validated, prehashed manifest using the production application writer.
//! The independent SDK consumer receives only this JSON output, never this
//! example's application dependencies or feature unification.

use std::io::Write;

use anyhow::{Context, Result};
use bitrouter::storage::checkpoint::validation::HarnessManifestExt;
use bitrouter_ai::core::protocol::{
    Capabilities, HarnessManifest, HarnessTool, Limits, ToolEffect, VERSION,
};
use serde_json::json;

fn main() -> Result<()> {
    let tools = vec![HarnessTool {
        name: "read_file".into(),
        description: "Read a bounded portion of a workspace file".into(),
        // Both this object and its properties deliberately use nonalphabetical
        // order. The production digest commits to these serialized bytes.
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Workspace-relative path"},
                "max_bytes": {"type": "integer", "minimum": 1, "maximum": 4096}
            },
            "required": ["path"],
            "additionalProperties": false
        }),
        effect: ToolEffect::Read,
        approval_required: false,
    }];
    // HarnessManifestExt::digest serializes this same typed slice with to_vec
    // before calling the application's SHA-256 implementation. Keep the exact
    // preimage as a string so the consuming parser cannot reorder the oracle.
    let tools_json = String::from_utf8(serde_json::to_vec(&tools)?)?;
    let manifest = HarnessManifest {
        tool_manifest_digest: HarnessManifest::digest(&tools)?,
        tools,
        workspace_id: "manifest_fidelity_workspace".into(),
        workspace_revision: Some("workspace_revision_1".into()),
        permission_revision: 1,
        max_tool_output_bytes: 4096,
        artifact_quota_bytes: 64 * 1024,
        max_artifact_chunk_bytes: 4096,
        required_features: vec!["session.bind".into()],
    };
    let limits = Limits::default();
    let capabilities = Capabilities {
        version: VERSION,
        core_instance_id: "manifest_fidelity_core".into(),
        operations: vec!["session.bind".into()],
        transports: vec!["websocket".into()],
        unsupported_features: Vec::new(),
        limits: limits.clone(),
        max_sessions: 1,
        max_host_model_attempts: 1,
    };
    manifest.validate(&capabilities, &limits)?;

    let artifact = bitrouter::artifact::current();
    let writer_revision = artifact
        .source_revision
        .clone()
        .context("manifest fixture writer requires a known source revision")?;
    let fixture = json!({
        "writer": "bitrouter::storage::checkpoint::validation::HarnessManifestExt::digest",
        "writer_revision": writer_revision,
        "writer_artifact": artifact,
        "tools_json": tools_json,
        "manifest_json": serde_json::to_string(&manifest)?,
        "tool_manifest_digest": manifest.tool_manifest_digest
    });
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &fixture)?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}
