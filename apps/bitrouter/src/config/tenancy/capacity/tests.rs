use super::CapacityConfig;

fn field(path: &str, value: serde_json::Value) -> anyhow::Result<CapacityConfig> {
    let (group, key) = path
        .split_once('.')
        .ok_or_else(|| anyhow::anyhow!("field path"))?;
    Ok(serde_json::from_value(
        serde_json::json!({group: {key: value}}),
    )?)
}

#[test]
fn omitted_budgets_preserve_defaults_and_reject_unknown_fields() -> anyhow::Result<()> {
    let defaults: CapacityConfig = serde_json::from_str("{}")?;
    assert_eq!(defaults, CapacityConfig::default());
    defaults.validate()?;
    for input in [
        r#"{"tenants":10000}"#,
        r#"{"execution":{"unlimited":true}}"#,
    ] {
        assert!(serde_json::from_str::<CapacityConfig>(input).is_err());
    }
    let partial: CapacityConfig = serde_json::from_str(r#"{"caches":{"execution_entries":1024}}"#)?;
    assert_eq!(partial.caches.execution_entries, 1024);
    assert_eq!(
        partial.caches.execution_bytes,
        defaults.caches.execution_bytes
    );
    assert_eq!(
        partial.caches.credential_bytes,
        defaults.caches.credential_bytes
    );
    Ok(())
}

#[test]
fn zero_is_only_a_queue_disable_and_large_counts_are_not_product_limits() -> anyhow::Result<()> {
    for path in [
        "execution.instance_active",
        "execution.tenant_active",
        "execution.queue_wait_ms",
        "ingress.requests",
        "ingress.bytes",
        "management.queue",
        "management.safety_queue",
        "management.reads",
        "caches.execution_entries",
        "caches.execution_bytes",
        "caches.credential_entries",
        "caches.credential_bytes",
        "authority_views.entries",
        "authority_views.bytes",
        "routing.entries",
        "routing.bytes",
        "receipts.high_water_bytes",
        "receipts.admission_free_bytes",
        "receipts.maintenance_free_bytes",
        "receipts.unknown_instance",
        "receipts.unknown_tenant",
        "transport.connections",
        "transport.header_timeout_ms",
    ] {
        assert!(field(path, 0.into())?.validate().is_err(), "{path}");
    }
    let mut config = CapacityConfig::default();
    config.execution.instance_queue = 0;
    config.execution.tenant_queue = 0;
    config.validate()?;
    for path in [
        "execution.instance_active",
        "ingress.requests",
        "management.queue",
        "management.reads",
        "transport.connections",
    ] {
        field(path, 131_072.into())?.validate()?;
    }
    Ok(())
}

#[test]
fn reject_overflow_and_inconsistent_resource_relations_before_construction() -> anyhow::Result<()> {
    for path in [
        "execution.instance_active",
        "management.queue",
        "transport.connections",
        "ingress.bytes",
        "routing.bytes",
    ] {
        assert!(
            field(path, serde_json::json!(usize::MAX))?
                .validate()
                .is_err(),
            "{path}"
        );
    }
    assert!(
        field("execution.instance_active", 1.into())?
            .validate()
            .is_err()
    );
    assert!(
        field("execution.instance_queue", 1.into())?
            .validate()
            .is_err()
    );
    assert!(
        field("receipts.unknown_instance", 1.into())?
            .validate()
            .is_err()
    );
    assert!(
        field("receipts.admission_free_bytes", 1.into())?
            .validate()
            .is_err()
    );
    assert!(
        field("receipts.high_water_bytes", serde_json::json!(u64::MAX))?
            .validate()
            .is_err()
    );
    let mut config = CapacityConfig::default();
    config.caches.execution_bytes = isize::MAX as usize;
    assert!(config.validate().is_err());
    for value in [serde_json::json!(-1), serde_json::json!(1.5)] {
        assert!(field("ingress.requests", value).is_err());
    }
    Ok(())
}
