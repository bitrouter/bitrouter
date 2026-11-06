use super::*;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;

fn scratch() -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?)
}

fn lines<'a>(bytes: &[u8], workspace: &'a Workspace) -> Result<JsonLines<'a>> {
    let mut file = tempfile::tempfile()?;
    file.write_all(bytes)?;
    file.seek(SeekFrom::Start(0))?;
    JsonLines::new(file, workspace)
}

fn header(operations: u64) -> String {
    format!(
        "{{\"format\":\"{}\",\"installation_id\":\"test-installation\",\"operation_count\":{operations},\"configuration_count\":0,\"active_count\":0,\"resource_count\":0}}\n",
        format::PROVISIONING
    )
}

fn operation(revision: u64) -> String {
    format!(
        "{{\"kind\":\"operation\",\"principal_id\":\"operator.admin\",\"result\":{{\"operation_id\":\"operation-{revision}\",\"status\":\"applied\",\"authority_revision\":{revision},\"resource_revision\":\"revision-{revision}\",\"resource_id\":\"tenant\",\"error\":null}}}}\n"
    )
}

#[test]
fn complete_stream_rejects_truncation_trailing_records_and_revision_gaps() -> Result<()> {
    let parent = scratch()?;
    let workspace = Workspace::create(
        &crate::offline::large::Options {
            working_bytes: 1024 * 1024,
            scratch_directory: parent.path().to_path_buf(),
            min_free_bytes: 1,
        },
        tokio_util::sync::CancellationToken::new(),
    )?;
    let check = || Ok(());
    let installation = "test-installation".try_into()?;
    let valid = header(2) + &operation(1) + &operation(2);
    let mut parsed =
        Provisioning::new(lines(valid.as_bytes(), &workspace)?, &installation, &check)?;
    assert!(parsed.next(&check)?.is_some());
    assert!(parsed.next(&check)?.is_some());
    assert!(parsed.next(&check)?.is_none());
    for invalid in [
        header(2) + &operation(1),
        header(2) + &operation(2),
        header(1) + &operation(1) + &operation(2),
        valid.trim_end_matches('\n').to_owned(),
    ] {
        let mut parsed = Provisioning::new(
            lines(invalid.as_bytes(), &workspace)?,
            &installation,
            &check,
        )?;
        let result = (|| -> Result<()> {
            while parsed.next(&check)?.is_some() {}
            Ok(())
        })();
        assert!(result.is_err());
    }
    Ok(())
}

#[test]
fn parser_refuses_unknown_fields_duplicate_fields_and_predecode_capacity() -> Result<()> {
    let parent = scratch()?;
    let workspace = Workspace::create(
        &crate::offline::large::Options {
            working_bytes: 1024 * 1024,
            scratch_directory: parent.path().to_path_buf(),
            min_free_bytes: 1,
        },
        tokio_util::sync::CancellationToken::new(),
    )?;
    let check = || Ok(());
    for value in [
        b"{\"id\":\"a\",\"extra\":1}\n".as_slice(),
        b"{\"id\":\"a\",\"id\":\"b\"}\n".as_slice(),
    ] {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Record {
            id: String,
        }
        let result = lines(value, &workspace)?.next::<Record>(&check);
        assert!(result.is_err());
        // Keep the test DTO's field part of a real read, without dead fields.
        let good = lines(b"{\"id\":\"a\"}\n", &workspace)?
            .next::<Record>(&check)?
            .context("record")?;
        assert_eq!(good.value.id, "a");
    }
    let dense = format!("[{}0]\n", "0,".repeat(8192));
    let error = lines(dense.as_bytes(), &workspace)?
        .next::<Vec<u64>>(&check)
        .err()
        .context("capacity refusal")?;
    assert!(error.is::<WorkingCapacity>() || error.is::<crate::offline::large::CapacityExceeded>());
    Ok(())
}

#[test]
fn fixed_resource_keys_use_sql_string_order_and_identity_padding_is_injective() -> Result<()> {
    use bitrouter_ai::tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
    };
    let reference = |owner_kind, resource_kind| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind,
            owner_id: "owner".try_into()?,
            resource_kind,
            resource_id: "resource".try_into()?,
            revision_id: "revision".try_into()?,
        })
    };
    assert!(
        format::resource_key(&reference(OwnerKind::OperatorPool, ResourceKind::Route)?)?
            < format::resource_key(&reference(OwnerKind::Tenant, ResourceKind::Route)?)?
    );
    assert!(
        format::resource_key(&reference(OwnerKind::Tenant, ResourceKind::Checkpoint)?)?
            < format::resource_key(&reference(OwnerKind::Tenant, ResourceKind::Credential)?)?
    );
    assert!(format::padded::<128>("a")? < format::padded::<128>("aa")?);
    assert_ne!(format::padded::<256>("é")?, format::padded::<256>("e")?);
    assert!(format::padded::<256>("writer\0").is_err());
    assert!(format::label("writer\0").is_err());
    assert!(format::label(&"é".repeat(129)).is_err());
    Ok(())
}

#[test]
fn tagged_records_preserve_raw_integer_semantics_and_strict_fields() -> Result<()> {
    let parent = scratch()?;
    let workspace = Workspace::create(
        &crate::offline::large::Options {
            working_bytes: 1024 * 1024,
            scratch_directory: parent.path().to_path_buf(),
            min_free_bytes: 1,
        },
        tokio_util::sync::CancellationToken::new(),
    )?;
    let check = || Ok(());
    let active = |number: &str| {
        format!(
            "{{\"tenant_id\":\"tenant\",\"revision_id\":\"r1\",\"authority_revision\":{number},\"kind\":\"active\"}}\n"
        )
    };
    for number in ["1", "1.0", "1e0", "9007199254740991"] {
        let record = lines(active(number).as_bytes(), &workspace)?
            .next::<ProvisioningRecord>(&check)?
            .context("active record")?;
        assert!(matches!(record.value, ProvisioningRecord::Active { .. }));
    }
    for number in [
        "-1",
        "1.00000000000000000001",
        "9007199254740990.9",
        "9007199254740992",
        "1e20",
    ] {
        assert!(
            lines(active(number).as_bytes(), &workspace)?
                .next::<ProvisioningRecord>(&check)
                .is_err()
        );
    }
    // Nested configuration numbers use the same original wire decoder too.
    let configuration = format!(
        "{{\"kind\":\"configuration\",\"revision\":{{\"tenant_id\":\"tenant\",\"revision_id\":\"r1\",\"schema_version\":1,\"compiler_version\":\"fixture\",\"content_digest\":\"{}\",\"document\":{{\"routes\":[],\"policy\":null,\"generation_defaults\":{{\"max_output_tokens\":128.0,\"temperature\":0.2}},\"learning\":{{\"mode\":\"off\",\"max_evaluations\":0}},\"limits\":{{\"active_executions\":1,\"queued_work\":1,\"learning_jobs\":0}}}}}}}}\n",
        "a".repeat(64)
    );
    assert!(matches!(
        lines(configuration.as_bytes(), &workspace)?
            .next::<ProvisioningRecord>(&check)?
            .context("configuration record")?
            .value,
        ProvisioningRecord::Configuration { .. }
    ));
    for value in [
        active("1").replace(
            "\"kind\":\"active\"",
            "\"kind\":\"active\",\"kind\":\"active\"",
        ),
        active("1").replace("\"kind\":\"active\"", "\"kind\":\"active\",\"extra\":null"),
        active("1").replace(
            "\"kind\":\"active\"",
            "\"kind\":\"active\",\"principal_id\":\"caller\"",
        ),
        operation(1).replace(
            "\"status\":\"applied\"",
            "\"status\":\"applied\",\"extra\":0",
        ),
        operation(1).replace(
            "\"authority_revision\":1",
            "\"authority_revision\":1,\"authority_revision\":1",
        ),
        configuration.replace("128.0", "128.00000000000000000001"),
    ] {
        assert!(
            lines(value.as_bytes(), &workspace)?
                .next::<ProvisioningRecord>(&check)
                .is_err()
        );
    }
    Ok(())
}
