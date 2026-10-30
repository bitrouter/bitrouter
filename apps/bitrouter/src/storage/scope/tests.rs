use super::*;

fn owned(tenant: &str, creator: &str) -> Result<(Scope, Provenance)> {
    Ok(Scope::for_test(
        "installation".try_into()?,
        tenant.try_into()?,
        creator.try_into()?,
    ))
}

#[test]
fn keys_are_exact_domain_separated_and_creator_independent() -> Result<()> {
    let (a, first) = owned("tenant", "first")?;
    let (again, second) = owned("tenant", "second")?;
    let (b, _) = owned("Tenant", "first")?;
    assert_eq!(a, again);
    assert_ne!(first, second);
    let key = a.physical_key(KeyDomain::EvalSubject, "legacy", &["logical"])?;
    assert_eq!(key.len(), 64);
    assert!(
        key.bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    );
    assert_eq!(
        key,
        again.physical_key(KeyDomain::EvalSubject, "ignored", &["logical"])?
    );
    assert_ne!(
        key,
        b.physical_key(KeyDomain::EvalSubject, "legacy", &["logical"])?
    );
    assert_ne!(
        key,
        a.physical_key(KeyDomain::EvalResult, "legacy", &["logical"])?
    );
    let parts = [["a", "bc"], ["ab", "c"]];
    assert_ne!(
        a.physical_key(KeyDomain::EvalSubject, "", &parts[0])?,
        a.physical_key(KeyDomain::EvalSubject, "", &parts[1])?
    );
    let keys = ["Case", "case", "case ", "é", "e\u{301}"]
        .iter()
        .map(|id| a.physical_key(KeyDomain::EvalSubject, id, &[id]))
        .collect::<Result<std::collections::BTreeSet<_>>>()?;
    assert_eq!(keys.len(), 5);
    assert_eq!(
        Scope::legacy().physical_key(KeyDomain::EvalSubject, " \u{301}:legacy ", &[])?,
        " \u{301}:legacy "
    );
    Ok(())
}

#[test]
fn metadata_is_not_authority_and_rejects_partial_namespaces() -> Result<()> {
    let (scope, provenance) = owned("tenant", "principal")?;
    let row = scope.metadata(&provenance)?;
    assert_eq!(
        scope.validate_metadata(MetadataRef {
            installation: row.installation.as_deref(),
            tenant: row.tenant.as_deref(),
            creator: row.creator.as_deref()
        })?,
        provenance
    );
    assert!(
        scope
            .validate_metadata(MetadataRef {
                installation: Some("installation"),
                tenant: Some("tenant"),
                creator: None
            })
            .is_err()
    );
    assert!(
        scope
            .validate_metadata(MetadataRef {
                installation: None,
                tenant: Some("tenant"),
                creator: Some("principal")
            })
            .is_err()
    );
    assert!(
        scope
            .validate_metadata(MetadataRef {
                installation: Some("other"),
                tenant: Some("tenant"),
                creator: Some("principal")
            })
            .is_err()
    );
    assert!(
        scope
            .validate_metadata(MetadataRef {
                installation: Some("installation"),
                tenant: Some("tenant"),
                creator: Some("invalid principal")
            })
            .is_err()
    );
    assert!(
        Scope::legacy()
            .validate_metadata(MetadataRef {
                installation: None,
                tenant: None,
                creator: Some("principal")
            })
            .is_err()
    );
    assert!(scope.metadata(&Provenance::legacy()).is_err());
    Ok(())
}
