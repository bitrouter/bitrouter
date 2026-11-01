use bitrouter_ai::tenancy::{
    directory::{DirectoryCursorV1, DirectoryEntryV1, DirectoryPageV1},
    validation::Validate,
};
use serde_json::{Value, json};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn page() -> Value {
    json!({"schema_version":1,"installation_id":"installation","authority_revision":9,"entries":[
        {"kind":"binding","tenant_id":"tenant","binding_id":"binding","revision_id":"b1","principal_id":"principal","role":"viewer","scope":{"kind":"tenant"},"expires_at":"2030-01-01T00:00:00Z","management_owner":{"principal_id":"operator.synthetic","authority_class":"installation"}},
        {"kind":"credential","tenant_id":"tenant","credential_id":"credential","revision_id":"c1","principal_id":"principal","binding_id":"binding","permissions":["resource.read"],"scope":{"kind":"tenant"},"expires_at":"2020-01-01T00:00:00Z","recorded_state":"active","audit_reader":false},
        {"kind":"principal","principal_id":"principal","revision_id":"p1","issuer":"sensitive-issuer","subject":"sensitive-subject"},
        {"kind":"tenant","tenant_id":"tenant","revision_id":"t1","state":"suspended"}
    ],"next_after":null})
}

#[test]
fn directory_contract_roundtrip_nullable_states_and_redacted_diagnostics() -> TestResult {
    let value = page();
    let typed: DirectoryPageV1 = serde_json::from_value(value.clone())?;
    typed.validate()?;
    assert_eq!(serde_json::to_value(&typed)?, value);
    assert_eq!(format!("{typed:?}"), "DirectoryPageV1 { .. }");
    for entry in &typed.entries {
        assert_eq!(format!("{entry:?}"), "DirectoryEntryV1 { .. }");
        let cursor = entry.cursor();
        assert_eq!(cursor.key(), entry.key());
        assert_eq!(format!("{cursor:?}"), "DirectoryCursorV1 { .. }");
        assert_eq!(
            serde_json::from_str::<DirectoryCursorV1>(&serde_json::to_string(&cursor)?)?,
            cursor
        );
    }
    let mut audit = value["entries"][1].clone();
    audit["audit_reader"] = true.into();
    audit["binding_id"] = Value::Null;
    audit["permissions"] = json!([]);
    audit["recorded_state"] = "revoked".into();
    serde_json::from_value::<DirectoryEntryV1>(audit.clone())?.validate()?;
    audit["permissions"] = json!(["receipt.read"]);
    assert!(
        serde_json::from_value::<DirectoryEntryV1>(audit)?
            .validate()
            .is_err()
    );
    for (index, field) in [(0, "management_owner"), (1, "binding_id")] {
        let mut entry = value["entries"][index].clone();
        entry.as_object_mut().ok_or("object")?.remove(field);
        assert!(serde_json::from_value::<DirectoryEntryV1>(entry).is_err());
    }
    let mut missing = value.clone();
    missing
        .as_object_mut()
        .ok_or("object")?
        .remove("next_after");
    assert!(serde_json::from_value::<DirectoryPageV1>(missing).is_err());
    for field in ["secret", "key_hash", "ciphertext", "resource_plaintext"] {
        let mut extra = value["entries"][1].clone();
        extra[field] = "must-never-return".into();
        assert!(serde_json::from_value::<DirectoryEntryV1>(extra).is_err());
    }
    Ok(())
}

#[test]
fn directory_cursor_and_page_invariants_reject_mixed_or_ambiguous_scans() -> TestResult {
    for invalid in [
        "",
        "v2|tenant||t",
        "v1|tenant|t|t",
        "v1|credential||c",
        "v1|binding|t|b|extra",
        "v1|principal||雪",
        "v1|unknown||id",
        "v1|principal||sensitive/subject",
    ] {
        let error = DirectoryCursorV1::try_from(invalid)
            .err()
            .ok_or("cursor accepted")?;
        assert_eq!(
            error.to_string(),
            "invalid tenancy contract: directory cursor"
        );
    }
    assert!(
        DirectoryCursorV1::try_from("v1|credential|a|z")?
            < DirectoryCursorV1::try_from("v1|credential|aa|a")?
    );
    let mut valid: DirectoryPageV1 = serde_json::from_value(page())?;
    valid.next_after = valid.entries.last().map(DirectoryEntryV1::cursor);
    valid.validate()?;
    let mut invalid = valid.clone();
    invalid.entries.swap(0, 1);
    assert!(invalid.validate().is_err());
    let mut invalid = valid.clone();
    invalid.entries.push(valid.entries[3].clone());
    assert!(invalid.validate().is_err());
    let mut invalid = valid.clone();
    invalid.next_after = Some(valid.entries[0].cursor());
    assert!(invalid.validate().is_err());
    let mut invalid = valid.clone();
    invalid.schema_version = 2;
    assert!(invalid.validate().is_err());
    valid.entries.clear();
    assert!(valid.validate().is_err());
    valid.next_after = None;
    valid.validate()?;
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn directory_schema_matches_public_contract_and_preserves_required_nulls() -> TestResult {
    let schema = schemars::schema_for!(DirectoryPageV1);
    let rendered = format!("{}\n", serde_json::to_string_pretty(&schema)?);
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/specs/service-api/directory.schema.json");
    if std::env::var("BITROUTER_BLESS").as_deref() == Ok("1") {
        std::fs::write(&path, &rendered)?;
    }
    assert_eq!(std::fs::read_to_string(path)?, rendered);
    let value = serde_json::to_value(schema)?;
    assert!(
        value["required"]
            .as_array()
            .ok_or("required")?
            .contains(&json!("next_after"))
    );
    assert!(
        value["$defs"]["RequiredNullable_DirectoryCursorV1"]
            .to_string()
            .contains("null")
    );
    assert_eq!(value["$defs"]["DirectoryCursorV1"]["maxLength"], 271);
    Ok(())
}
