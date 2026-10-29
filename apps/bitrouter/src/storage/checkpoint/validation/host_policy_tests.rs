use super::{HarnessManifestExt, limits_within, negotiate_capabilities};
use bitrouter_ai::core::protocol::{
    Capabilities, CommitStatus, CoreError, ErrorCode, HarnessManifest, Limits, VERSION,
};
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn caps() -> Capabilities {
    Capabilities {
        version: VERSION,
        core_instance_id: "core_1".into(),
        operations: vec!["session.bind".into(), "checkpoint.ack".into()],
        transports: vec!["in_process".into()],
        unsupported_features: vec![
            "response.inject".into(),
            "openai_encrypted_agent_message".into(),
        ],
        limits: Limits::default(),
        max_sessions: 16,
        max_host_model_attempts: 16,
    }
}

fn rejected(code: ErrorCode, message: &str) -> Result<(), CoreError> {
    Err(CoreError {
        code,
        message: message.into(),
        commit_status: CommitStatus::NotCommitted,
    })
}

#[test]
fn capabilities_reject_unsupported_requirements_and_escalated_limits() -> TestResult {
    let capabilities = caps();
    negotiate_capabilities(
        &capabilities,
        1,
        &["session.bind".into()],
        &Limits::default(),
    )?;
    assert_eq!(
        negotiate_capabilities(&capabilities, 2, &[], &Limits::default()),
        rejected(
            ErrorCode::UnsupportedVersion,
            "unsupported harness protocol version",
        ),
    );
    assert_eq!(
        negotiate_capabilities(
            &capabilities,
            1,
            &["response.inject".into()],
            &Limits::default(),
        ),
        rejected(
            ErrorCode::UnsupportedCapability,
            "unsupported required feature: response.inject",
        ),
    );
    let raised = Limits {
        active_models: 5,
        ..Limits::default()
    };
    assert_eq!(
        negotiate_capabilities(&capabilities, 1, &[], &raised),
        rejected(
            ErrorCode::LimitExceeded,
            "requested limits exceed host policy"
        ),
    );
    assert_eq!(
        Limits {
            active_models: 0,
            ..Limits::default()
        }
        .validate(),
        rejected(ErrorCode::LimitExceeded, "invalid execution or byte limits"),
    );
    Ok(())
}

#[test]
fn negotiation_preserves_version_capability_and_limit_error_order() {
    let mut capabilities = caps();
    capabilities.limits.active_models = 0;
    let invalid = Limits {
        input_bytes: 0,
        ..Limits::default()
    };
    let required = [
        "session.bind".into(),
        "unknown_z".into(),
        "response.inject".into(),
    ];
    // Both matching unsupported versions and either side's mismatch fail first.
    for (version, host_version) in [(2, 1), (1, 2), (2, 2)] {
        capabilities.version = host_version;
        assert_eq!(
            negotiate_capabilities(&capabilities, version, &required, &invalid),
            rejected(
                ErrorCode::UnsupportedVersion,
                "unsupported harness protocol version",
            ),
        );
    }
    capabilities.version = VERSION;
    assert_eq!(
        negotiate_capabilities(&capabilities, VERSION, &required, &invalid),
        rejected(
            ErrorCode::UnsupportedCapability,
            "unsupported required feature: unknown_z",
        ),
    );
    assert_eq!(
        negotiate_capabilities(&capabilities, VERSION, &[], &invalid),
        rejected(ErrorCode::LimitExceeded, "invalid execution or byte limits"),
    );
}

#[test]
fn limits_enforce_each_dimension_at_the_host_boundary() -> TestResult {
    // A finite cumulative tool budget exercises escalation below the wire default.
    let host = Limits {
        total_tools: 256,
        ..Limits::default()
    };
    limits_within(&host, &host)?;
    let wire = serde_json::to_value(&host)?;
    for field in [
        "active_models",
        "agents",
        "child_depth",
        "model_attempts",
        "outstanding_tools",
        "total_tools",
        "queued_runs",
        "mailbox_messages",
        "active_seconds",
        "checkpoint_bytes",
        "unacknowledged_bytes",
        "ephemeral_bytes",
        "input_bytes",
    ] {
        let bound = wire[field].as_u64().ok_or("missing numeric limit")?;
        let mut lowered = wire.clone();
        lowered[field] = json!(bound - 1);
        let lowered: Limits = serde_json::from_value(lowered)?;
        assert_eq!(limits_within(&lowered, &host), Ok(()), "{field}");

        let mut raised = wire.clone();
        raised[field] = json!(bound + 1);
        let raised: Limits = serde_json::from_value(raised)?;
        // These are valid contracts: only comparison with host policy rejects them.
        raised.validate()?;
        assert_eq!(
            limits_within(&raised, &host),
            rejected(
                ErrorCode::LimitExceeded,
                "requested limits exceed host policy"
            ),
            "{field}",
        );
    }
    Ok(())
}

#[test]
fn invalid_requested_or_host_limits_precede_policy_comparison() {
    let valid = Limits::default();
    for invalid in [
        Limits {
            active_models: 0,
            ..valid.clone()
        },
        Limits {
            checkpoint_bytes: 4095,
            input_bytes: 1023,
            ..valid.clone()
        },
        Limits {
            unacknowledged_bytes: valid.checkpoint_bytes - 1,
            ..valid.clone()
        },
        Limits {
            input_bytes: valid.checkpoint_bytes / 4 + 1,
            ..valid.clone()
        },
    ] {
        assert_eq!(
            limits_within(&invalid, &valid),
            rejected(ErrorCode::LimitExceeded, "invalid execution or byte limits"),
        );
        assert_eq!(
            limits_within(&valid, &invalid),
            rejected(ErrorCode::LimitExceeded, "invalid execution or byte limits"),
        );
    }
}

#[test]
fn limits_accept_minimum_bounds_and_zero_optional_budgets() -> TestResult {
    let minimum = Limits {
        active_models: 1,
        agents: 1,
        child_depth: 0,
        model_attempts: 1,
        outstanding_tools: 1,
        total_tools: 0,
        queued_runs: 0,
        mailbox_messages: 1,
        active_seconds: 1,
        checkpoint_bytes: 4096,
        unacknowledged_bytes: 4096,
        ephemeral_bytes: 0,
        input_bytes: 1024,
    };
    limits_within(&minimum, &Limits::default())?;
    limits_within(&minimum, &minimum)?;
    Ok(())
}

#[test]
fn legacy_limits_keep_unlimited_tools_and_require_host_admission() -> TestResult {
    let host = Limits::default();
    let mut wire = serde_json::to_value(&host)?;
    wire.as_object_mut()
        .ok_or("expected limits object")?
        .remove("total_tools");
    let legacy: Limits = serde_json::from_value(wire)?;
    assert_eq!(legacy, host);
    assert_eq!(legacy.total_tools, u32::MAX);
    limits_within(&legacy, &host)?;
    let restricted = Limits {
        total_tools: 256,
        ..host
    };
    assert_eq!(
        limits_within(&legacy, &restricted),
        rejected(
            ErrorCode::LimitExceeded,
            "requested limits exceed host policy"
        ),
    );
    limits_within(&restricted, &legacy)?;
    Ok(())
}

#[test]
fn persisted_manifest_checks_host_policy_before_record_fields() -> TestResult {
    let capabilities = caps();
    let wire = json!({
        "tools": [],
        "tool_manifest_digest": HarnessManifest::digest(&[])?,
        "workspace_id": "workspace_1",
        "workspace_revision": null,
        "permission_revision": 1,
        "max_tool_output_bytes": 8192,
        "artifact_quota_bytes": 1048576,
        "max_artifact_chunk_bytes": 8192,
        "required_features": ["session.bind"]
    });
    let mut manifest: HarnessManifest = serde_json::from_value(wire.clone())?;
    assert_eq!(serde_json::to_value(&manifest)?, wire);
    manifest.validate(&capabilities, &capabilities.limits)?;

    // Host admission still precedes even invalid persisted identities and digests.
    manifest.workspace_id.clear();
    manifest.tool_manifest_digest.clear();
    manifest.required_features.push("response.inject".into());
    let raised = Limits {
        active_models: 5,
        ..Limits::default()
    };
    let mut unsupported = capabilities.clone();
    unsupported.version = 2;
    assert_eq!(
        manifest.validate(&unsupported, &raised),
        rejected(
            ErrorCode::UnsupportedVersion,
            "unsupported harness protocol version",
        ),
    );
    assert_eq!(
        manifest.validate(&capabilities, &raised),
        rejected(
            ErrorCode::UnsupportedCapability,
            "unsupported required feature: response.inject",
        ),
    );
    manifest.required_features.truncate(1);
    assert_eq!(
        manifest.validate(&capabilities, &raised),
        rejected(
            ErrorCode::LimitExceeded,
            "requested limits exceed host policy"
        ),
    );
    let mut invalid_host = capabilities.clone();
    invalid_host.limits.active_models = 0;
    assert_eq!(
        manifest.validate(&invalid_host, &capabilities.limits),
        rejected(ErrorCode::LimitExceeded, "invalid execution or byte limits"),
    );
    assert_eq!(
        manifest.validate(&capabilities, &capabilities.limits),
        rejected(
            ErrorCode::UnauthorizedScope,
            "invalid or oversized identity"
        ),
    );
    Ok(())
}
