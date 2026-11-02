use super::*;
use crate::tenancy::{
    authority::live::Authority,
    model::{AuthorityHeader, Snapshot},
};
use bitrouter_ai::tenancy::authority::OwnerKind;
use std::sync::{Arc, atomic::AtomicBool};

#[test]
fn fresh_switch_fits_one_dependency_budget_while_owner_union_is_preserved() -> anyhow::Result<()> {
    let header = AuthorityHeader {
        installation: "installation".try_into()?,
        revision: 1.try_into()?,
        operation_ids: 0,
        safety_operation_ids: 0,
        saturated: false,
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    let authority = Authority::new(
        header.clone(),
        &Default::default(),
        Arc::new(AtomicBool::new(true)),
    )
    .map_err(code)?;
    let a = ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "tenant".try_into()?,
        resource_kind: ResourceKind::Route,
        resource_id: "a".try_into()?,
        revision_id: "v1".try_into()?,
    };
    let b = ResourceRef {
        resource_id: "b".try_into()?,
        ..a.clone()
    };
    let mut facts = Snapshot::from_header(&header);
    facts.resource_lifecycle.absent(a.clone());
    let lease = authority
        .register(View::new(facts, Vec::new()).map_err(code)?, 0)
        .map_err(code)?;
    let subject = |intent| -> anyhow::Result<AuthoritySubject> {
        Ok(AuthoritySubject::Extend {
            caller: Caller::Credential("credential".try_into()?),
            lease: lease.clone(),
            dependencies: vec![b.clone()],
            intent,
        })
    };
    let one = 512 + std::mem::size_of::<ResourceRef>();
    let fresh = read_dependencies(&authority, &subject(DependencyIntent::Fresh)?, one)
        .map_err(|error| code(error.0.code))?;
    assert_eq!(fresh, vec![b.clone()]);
    assert!(
        matches!(read_dependencies(&authority, &subject(DependencyIntent::Owner)?, one), Err(error) if error.0.code == ErrorCode::CapacityExceeded)
    );
    let union = read_dependencies(&authority, &subject(DependencyIntent::Owner)?, 2 * one)
        .map_err(|error| code(error.0.code))?;
    assert_eq!(union.len(), 2);
    assert!(union.contains(&a) && union.contains(&b));
    authority
        .read()
        .map_err(code)?
        .check(&lease, |view| {
            assert!(view.facts().resource_lifecycle.loaded(&a));
            assert!(!view.facts().resource_lifecycle.loaded(&b));
            Ok(())
        })
        .map_err(code)?;
    Ok(())
}

fn code(error: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("authority: {error:?}")
}
