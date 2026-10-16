//! Unmodified journals emitted by historical production writers.

use super::*;
use crate::harness::storage::{StoredExecution, validate_runtime_format};
use sha2::{Digest, Sha256};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn reconstruct(version: u32, records: u64, digest: &str) -> TestResult {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/runtime-history")
        .join(format!("runtime-v{version}.json.zst"));
    let bytes = zstd::stream::decode_all(std::fs::File::open(path)?)?;
    assert_eq!(hex::encode(Sha256::digest(&bytes)), digest);
    let stored: StoredExecution = serde_json::from_slice(&bytes)?;
    assert_eq!(stored.format_version, version);
    validate_runtime_format(stored.format_version)?;
    assert_eq!(stored.version, records);
    assert_eq!(u64::try_from(stored.records.len())?, records);
    let limits = crate::harness::service::default_runtime_limits();
    let mut rebuild = Rebuild::new(
        stored.records.first().ok_or("historical header missing")?,
        &stored.execution_id,
        limits.clone(),
    )?;
    rebuild.authorize(&CallerContext::local())?;
    // Decode only. Original workspace paths, identities and nested payloads
    // remain intact; no workspace is opened and no execution is authorized.
    for page in stored.records.chunks(7) {
        rebuild.consume_page(page.to_vec())?;
    }
    assert_eq!(rebuild.sequence, records);
    let (thread, _) = rebuild.finish("historical-offline-reader", &limits)?;
    assert!(thread.queued.is_empty());
    let view = thread.presentation.view;
    assert_eq!(view.thread.thread_id, stored.execution_id);
    assert_eq!(view.thread.cursor, records);
    assert_eq!(
        view.thread.context_version,
        if version == 6 { 5 } else { 4 }
    );
    let recovery = view.recovery.ok_or("missing recovery report")?;
    assert!(
        recovery.context_valid && recovery.terminal_checkpoint,
        "format {version}: {recovery:?}"
    );
    assert_eq!(recovery.stored_status, ThreadStatus::Paused);
    assert!(
        recovery
            .blockers
            .iter()
            .all(|blocker| matches!(blocker, RecoveryBlocker::OwnershipUnconfirmed)),
        "format {version}: {:?}",
        recovery.blockers
    );
    let turn = view.latest_turn.ok_or("missing historical turn")?;
    assert_eq!(turn.status, TurnStatus::Completed);
    assert_eq!(turn.final_answer.as_deref(), Some("done"));
    assert!(!turn.unknown_effect);
    Ok(())
}

#[test]
fn baseline_v2_reconstructs_original_tool_exchange() -> TestResult {
    reconstruct(
        2,
        34,
        "bf04cfe7d3b14d4d860352ae4318fff21195b46cdac15b8067c76883e1e5845d",
    )
}

#[test]
fn baseline_v3_reconstructs_original_resource_inventory() -> TestResult {
    reconstruct(
        3,
        36,
        "140a5a33dea225167f1de828909c01cd18b4e99b120191ec9779be867144b111",
    )
}

#[test]
#[cfg(unix)] // The authentic instruction snapshot records a Unix absolute cwd.
fn baseline_v4_reconstructs_original_instruction_context() -> TestResult {
    reconstruct(
        4,
        37,
        "0852bcc7dd3558eaa3e7da51d9fe095c34ba32740534f87dfc419567faf06623",
    )
}

#[test]
#[cfg(unix)] // Preserve the writer's path dialect and production path validation.
fn baseline_v6_reconstructs_original_core_checkpoints() -> TestResult {
    reconstruct(
        6,
        106,
        "a1ac93f2bf711c730acf02423dcdace8b564c56b4093505e0e740dfab80c8648",
    )
}
