//! Run in this standalone workspace with a fixture emitted by the application's
//! sdk_manifest_fixture example. Strings retain the writer's exact original
//! bytes even when this consumer's serde_json object representation differs.

use anyhow::{Context, Result, bail, ensure};
use bitrouter_ai::core::protocol::{
    Bind, ClientMessage, Command, HarnessManifest, Limits, OwnershipGrant, Restore, VERSION,
};
use serde_json::Value;

fn has_non_alphabetical_object(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.keys().zip(object.keys().skip(1)).any(|(a, b)| a > b)
                || object.values().any(has_non_alphabetical_object)
        }
        Value::Array(values) => values.iter().any(has_non_alphabetical_object),
        _ => false,
    }
}

fn check_manifest(manifest: &HarnessManifest, tools_json: &str, digest: &str) -> Result<()> {
    ensure!(
        manifest.tool_manifest_digest == digest,
        "consumer changed the server's committed digest"
    );
    ensure!(
        serde_json::to_string(&manifest.tools)? == tools_json,
        "consumer changed the exact tool bytes hashed by the application"
    );
    ensure!(
        manifest
            .tools
            .iter()
            .any(|tool| has_non_alphabetical_object(&tool.parameters)),
        "fixture must exercise nonalphabetical schema keys"
    );
    Ok(())
}

#[test]
#[ignore = "requires BITROUTER_TEST_MANIFEST_FIXTURE from the application sdk_manifest_fixture writer"]
fn server_prehashed_manifest_survives_isolated_bind_and_restore() -> Result<()> {
    let path = std::env::var_os("BITROUTER_TEST_MANIFEST_FIXTURE")
        .context("set BITROUTER_TEST_MANIFEST_FIXTURE to the application-generated JSON")?;
    let fixture: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    ensure!(
        fixture["writer"]
            == "bitrouter::storage::checkpoint::validation::HarnessManifestExt::digest",
        "fixture must identify the actual application digest writer"
    );
    let writer_revision = fixture["writer_revision"]
        .as_str()
        .context("missing writer source revision")?;
    ensure!(!writer_revision.is_empty(), "empty writer source revision");
    ensure!(
        fixture["writer_artifact"]["source_revision"] == writer_revision,
        "writer revision must match its application build identity"
    );
    let tools_json = fixture["tools_json"]
        .as_str()
        .context("missing original tool bytes")?;
    let digest = fixture["tool_manifest_digest"]
        .as_str()
        .context("missing server digest")?;
    ensure!(
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "missing precomputed SHA-256 digest"
    );
    let manifest: HarnessManifest = serde_json::from_str(
        fixture["manifest_json"]
            .as_str()
            .context("missing original manifest bytes")?,
    )?;
    check_manifest(&manifest, tools_json, digest)?;

    let binding = Bind {
        grant: OwnershipGrant {
            session_id: "manifest_fidelity_session".into(),
            harness_id: "manifest_fidelity_harness".into(),
            core_instance_id: "manifest_fidelity_core".into(),
            execution_epoch: 1,
        },
        durable_head: Default::default(),
        checkpoint: None,
        manifest,
        limits: Limits::default(),
    };
    for command in [
        Command::Bind(Box::new(binding.clone())),
        Command::Restore(Box::new(Restore {
            binding,
            journal_tail: Vec::new(),
            tools: Vec::new(),
            results: Vec::new(),
            available_artifacts: Vec::new(),
            previous_owner_stopped: true,
            active_time: None,
        })),
    ] {
        let message = ClientMessage {
            version: VERSION,
            session_id: "manifest_fidelity_session".into(),
            execution_epoch: 1,
            operation_id: "manifest_fidelity_operation".into(),
            expected_state_revision: None,
            command,
        };
        // Match the SDK channel's serde writer, without enabling an application
        // dependency that would unify preserve_order and conceal the regression.
        let mut wire = Vec::new();
        serde_json::to_writer(&mut wire, &message)?;
        let decoded: ClientMessage = serde_json::from_slice(&wire)?;
        let manifest = match &decoded.command {
            Command::Bind(binding) => &binding.manifest,
            Command::Restore(restore) => &restore.binding.manifest,
            _ => bail!("unexpected command after the client round trip"),
        };
        check_manifest(manifest, tools_json, digest)?;
    }
    Ok(())
}
