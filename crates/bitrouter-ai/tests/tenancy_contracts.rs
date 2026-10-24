use bitrouter_ai::tenancy::{
    authority::{CredentialIssued, GrantScope, Permission, TenantPut, TenantRole},
    configuration::Configuration,
    identity::{AuthorityRevision, TenantId, Timestamp},
    operation::{DispatchBinding, TenantCapabilities},
    validation::Validate,
};
use serde_json::json;

#[test]
fn omitted_cas_and_duplicate_fields_do_not_become_valid_commands()
-> Result<(), Box<dyn std::error::Error>> {
    let valid = json!({"operation_id":"op", "expected_revision":null, "state":"active", "installation_id":"install"});
    serde_json::from_value::<TenantPut>(valid.clone())?.validate()?;
    let mut omitted = valid;
    omitted
        .as_object_mut()
        .ok_or("object")?
        .remove("expected_revision");
    assert!(serde_json::from_value::<TenantPut>(omitted).is_err());
    assert!(serde_json::from_str::<TenantPut>(r#"{"operation_id":"op","operation_id":"different","expected_revision":null,"state":"active","installation_id":"install"}"#).is_err());
    assert!(
        serde_json::from_str::<GrantScope>(r#"{"kind":"resources","resources":[],"resources":[]}"#)
            .is_err()
    );
    assert!(
        serde_json::from_str::<GrantScope>(
            r#"{"kind":"tenant","kind":"resources","resources":[]}"#
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn ids_are_ascii_bounded_and_json_revisions_are_exact() -> Result<(), Box<dyn std::error::Error>> {
    for bad in ["", "../tenant", "t/1", "t%2f1", "租户", "-tenant", "t\n1"] {
        assert!(TenantId::try_from(bad).is_err());
    }
    assert!(TenantId::try_from("a".repeat(128)).is_ok());
    assert!(TenantId::try_from("a".repeat(129)).is_err());
    assert_eq!(
        TenantId::try_from("tenant:one._-1")?.as_str(),
        "tenant:one._-1"
    );
    assert!(AuthorityRevision::try_from(AuthorityRevision::MAX).is_ok());
    assert!(AuthorityRevision::try_from(AuthorityRevision::MAX + 1).is_err());
    assert!(serde_json::from_str::<AuthorityRevision>("-1").is_err());
    assert!(serde_json::from_str::<AuthorityRevision>("1.5").is_err());
    Ok(())
}

#[test]
fn integer_tokens_accept_exact_decimal_forms_without_float_rounding()
-> Result<(), Box<dyn std::error::Error>> {
    for (raw, expected) in [
        ("1.0", 1),
        ("1e0", 1),
        ("1.25e2", 125),
        ("12500e-2", 125),
        ("0.0100e2", 1),
        ("-0.0", 0),
        ("0e999999999999", 0),
        ("9007199254740991.000", AuthorityRevision::MAX),
    ] {
        assert_eq!(
            serde_json::from_str::<AuthorityRevision>(raw)?.get(),
            expected
        );
    }
    for raw in [
        "1.0000000000000000000001",
        "9007199254740991.1",
        "9007199254740992",
        "-0.00001",
        "1e-999999999999",
        "1e999999999999",
        "null",
        "\"1\"",
    ] {
        assert!(
            serde_json::from_str::<AuthorityRevision>(raw).is_err(),
            "invalid exact integer fixture"
        );
    }
    let wire = r#"{"input_tokens":1e0,"output_tokens":2.0,"cache_read_tokens":0.0,"cache_write_tokens":0,"reasoning_tokens":0,"provenance":"provider","complete":true}"#;
    let usage: bitrouter_ai::tenancy::execution::Usage = serde_json::from_str(wire)?;
    usage.validate()?;
    assert_eq!(usage.input_tokens, Some(1));
    assert_eq!(usage.output_tokens, Some(2));
    let same: bitrouter_ai::tenancy::execution::Usage =
        serde_json::from_value(serde_json::to_value(&usage)?)?;
    assert_eq!(same, usage);
    Ok(())
}

#[test]
fn utc_calendar_and_fraction_bounds_are_validated_without_a_runtime() {
    for good in [
        "2024-02-29T00:00:00Z",
        "2000-02-29t23:59:59.123456789123Z",
        "2016-12-31T23:59:60Z",
        "0000-01-01T00:00:00Z",
    ] {
        assert!(Timestamp::try_from(good).is_ok(), "valid UTC fixture");
    }
    for bad in [
        "2025-02-29T00:00:00Z",
        "1900-02-29T00:00:00Z",
        "2024-04-31T00:00:00Z",
        "2024-01-01T24:00:00Z",
        "2024-01-01T23:59:61Z",
        "2024-01-01T23:59:60Z",
        "2024-01-01T00:00:00.Z",
        "2024-01-01T00:00:00z",
        "2024-01-01T00:00:00+00:00",
        "🌏2024-01-01T00:00:00Z",
        "",
    ] {
        assert!(Timestamp::try_from(bad).is_err(), "invalid UTC fixture");
    }
}

#[test]
fn credential_debug_never_contains_the_one_time_secret() -> Result<(), Box<dyn std::error::Error>> {
    let response: CredentialIssued = serde_json::from_value(json!({
        "operation_id":"op", "credential_id":"credential", "principal_id":"principal", "tenant_id":"tenant",
        "authority_revision":1, "secret":"sensitive-canary", "expires_at":"2030-01-01T00:00:00Z", "credential_revision":"revision"
    }))?;
    response.validate()?;
    assert!(!format!("{response:?}").contains("sensitive-canary"));
    let mut absent = serde_json::to_value(response)?;
    absent.as_object_mut().ok_or("object")?.remove("secret");
    assert!(serde_json::from_value::<CredentialIssued>(absent).is_err());
    Ok(())
}

#[test]
fn resource_scope_is_explicit_and_operator_role_does_not_grant_execution()
-> Result<(), Box<dyn std::error::Error>> {
    assert!(
        serde_json::from_value::<GrantScope>(json!({"kind":"resources","resources":[]}))?
            .validate()
            .is_err()
    );
    assert!(serde_json::from_value::<GrantScope>(json!({"kind":"tenant","operator_pools":[{"owner_kind":"tenant","owner_id":"t","resource_kind":"route","resource_id":"r"}]}))?.validate().is_err());
    assert!(
        !TenantRole::TenantOperator
            .permissions()
            .contains(&Permission::ExecutionSubmit)
    );
    assert!(
        !TenantRole::Editor
            .permissions()
            .contains(&Permission::PolicyActivate)
    );
    assert!(
        TenantRole::TenantAdmin
            .permissions()
            .contains(&Permission::CredentialIssue)
    );
    Ok(())
}

#[test]
fn configuration_does_not_accept_host_fields_or_unbounded_references()
-> Result<(), Box<dyn std::error::Error>> {
    let mut wire = json!({"routes":[], "policy":null, "generation_defaults":{"max_output_tokens":1000,"temperature":1.0}, "learning":{"mode":"off","max_evaluations":0}, "limits":{"active_executions":16,"queued_work":32,"learning_jobs":2}});
    serde_json::from_value::<Configuration>(wire.clone())?.validate()?;
    wire["upstream_url"] = "https://untrusted.invalid".into();
    assert!(serde_json::from_value::<Configuration>(wire.clone()).is_err());
    wire.as_object_mut().ok_or("object")?.remove("upstream_url");
    let r = json!({"owner_kind":"tenant","owner_id":"tenant","resource_kind":"route","resource_id":"route","revision_id":"r1"});
    let mut second = r.clone();
    second["revision_id"] = "r2".into();
    wire["routes"] = json!([r, second]);
    assert!(
        serde_json::from_value::<Configuration>(wire)?
            .validate()
            .is_err()
    );
    Ok(())
}

#[test]
fn capability_binding_is_explicit_and_disabled_services_are_ineligible()
-> Result<(), Box<dyn std::error::Error>> {
    let mut cap: TenantCapabilities = serde_json::from_value(
        json!({"contract_version":1,"installation_id":"store","instance_id":"boot","capability_revision":"cap","revision_bound_dispatch":true,"strict_receipts":true,"dispatch_enabled":true}),
    )?;
    let binding = cap.bind("config".try_into()?, "correlation".try_into()?)?;
    binding.validate()?;
    let wire = serde_json::to_value(binding)?;
    assert_eq!(wire["Bitrouter-Instance-Id"], "boot");
    assert_eq!(wire["Bitrouter-Receipt-Mode"], "strict");
    serde_json::from_value::<DispatchBinding>(wire)?.validate()?;
    cap.dispatch_enabled = false;
    assert!(
        cap.bind("config".try_into()?, "correlation".try_into()?)
            .is_err()
    );
    Ok(())
}
