use super::*;
use crate::tenancy::model;
use bitrouter_ai::tenancy::authority::{
    CredentialState, GrantScope, Permission, ResourceKey, TenantRole,
};
use std::sync::atomic::Ordering;

fn credential(id: &str, keys: usize) -> anyhow::Result<model::Credential> {
    let mut resources = Vec::new();
    for index in 0..keys {
        resources.push(ResourceKey {
            owner_kind: OwnerKind::Tenant,
            owner_id: "a".try_into()?,
            resource_kind: ResourceKind::Route,
            resource_id: format!("{index:04}{}", "r".repeat(124)).try_into()?,
        });
    }
    Ok(model::Credential {
        id: id.try_into()?,
        tenant: "a".try_into()?,
        principal: "principal".try_into()?,
        binding: Some("binding".try_into()?),
        revision: "v1".try_into()?,
        key_hash: "a".repeat(64),
        state: CredentialState::Active,
        audit_reader: false,
        permissions: vec![Permission::ExecutionSubmit],
        scope: GrantScope::Resources { resources },
        expires: model::Deadline::parse("2030-01-01T00:00:00Z".try_into()?)?,
        created_at: header()?.created_at,
    })
}

fn credential_view(
    initial: &AuthorityHeader,
    records: &[model::Credential],
) -> anyhow::Result<View> {
    let mut facts = view(initial, "a")?.facts;
    for record in records {
        facts.credentials.insert(record.id.clone(), record.clone());
    }
    View::new(facts, Vec::new()).map_err(failure)
}

pub(super) fn full_authority(
    initial: &AuthorityHeader,
    views: Vec<View>,
) -> anyhow::Result<(Arc<Authority>, Vec<Lease>)> {
    let mut working = memory::registry(views.len()).map_err(failure)?;
    for view in &views {
        working = working
            .checked_add(view.bytes)
            .ok_or_else(|| anyhow::anyhow!("test budget"))?;
    }
    let budget = AuthorityViews {
        entries: views.len(),
        bytes: working
            .checked_mul(3)
            .ok_or_else(|| anyhow::anyhow!("test budget"))?,
    };
    let authority = Authority::new(initial.clone(), &budget, Arc::new(AtomicBool::new(true)))
        .map_err(failure)?;
    assert_eq!(authority.load_budget(), working);
    let leases = views
        .into_iter()
        .map(|view| authority.register(view, 0).map_err(failure))
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok((authority, leases))
}

#[test]
fn byte_budget_rounding_preserves_registration_and_publication_boundaries() -> anyhow::Result<()> {
    let initial = header()?;
    let working = memory::registry(1).map_err(failure)? + view(&initial, "a")?.bytes;
    for bytes in [3 * working - 1, 3 * working, 3 * working + 1] {
        let authority = Authority::new(
            initial.clone(),
            &AuthorityViews { entries: 1, bytes },
            Arc::new(AtomicBool::new(true)),
        )
        .map_err(failure)?;
        let registered = authority.register(view(&initial, "a")?, 0);
        if bytes < 3 * working {
            assert!(matches!(registered, Err(ErrorCode::CapacityExceeded)));
            assert!(
                authority
                    .state
                    .read()
                    .map_err(|_| failure(ErrorCode::AuthorityUnavailable))?
                    .views
                    .is_empty()
            );
        } else {
            let lease = registered.map_err(failure)?;
            let mut next = initial.clone();
            next.revision = 2.try_into()?;
            authority
                .prepare(next, &[], None)
                .map_err(failure)?
                .publish()
                .map_err(failure)?;
            authority
                .read()
                .map_err(failure)?
                .check(&lease, |view| {
                    assert_eq!(view.facts.revision.get(), 2);
                    Ok(())
                })
                .map_err(failure)?;
            drop(lease);
        }
    }
    Ok(())
}

#[test]
fn full_budget_large_scope_revoke_and_tenant_denial_update_every_lease() -> anyhow::Result<()> {
    let initial = header()?;
    let mut revoked = credential("credential", 256)?;
    let first = credential_view(&initial, std::slice::from_ref(&revoked))?;
    let second = credential_view(&initial, std::slice::from_ref(&revoked))?;
    let (authority, leases) = full_authority(&initial, vec![first, second])?;
    revoked.state = CredentialState::Revoked;
    revoked.revision = "v".repeat(128).try_into()?;
    let tenant = model::Tenant {
        id: "a".try_into()?,
        revision: "t".repeat(128).try_into()?,
        state: TenantState::Suspended,
        created_at: initial.created_at.clone(),
    };
    let mut next = initial;
    next.revision = 2.try_into()?;
    authority
        .prepare(
            next,
            &[
                Change::Credential(revoked.clone()),
                Change::Tenant(tenant.clone()),
            ],
            None,
        )
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    assert_eq!(authority.staged_clones.load(Ordering::Relaxed), 2);
    let mut guard = authority.read().map_err(failure)?;
    for lease in &leases {
        guard
            .check(lease, |view| {
                assert_eq!(
                    view.facts.credentials[&revoked.id].state,
                    CredentialState::Revoked
                );
                assert_eq!(view.facts.tenants[&tenant.id].state, TenantState::Suspended);
                assert_eq!(view.facts.revision.get(), 2);
                Ok(())
            })
            .map_err(failure)?;
    }
    Ok(())
}

#[test]
fn later_view_expansion_rejects_before_any_payload_clone_and_leaves_gate_usable()
-> anyhow::Result<()> {
    let initial = header()?;
    let small = credential("credential", 1)?;
    let first = view(&initial, "a")?;
    let second = credential_view(&initial, std::slice::from_ref(&small))?;
    let (authority, leases) = full_authority(&initial, vec![first, second])?;
    let tenant = model::Tenant {
        id: "a".try_into()?,
        revision: "v2".try_into()?,
        state: TenantState::Suspended,
        created_at: initial.created_at.clone(),
    };
    let mut next = initial.clone();
    next.revision = 2.try_into()?;
    assert!(matches!(
        authority.prepare(
            next,
            &[
                Change::Tenant(tenant.clone()),
                Change::Credential(credential("credential", 256)?),
            ],
            None
        ),
        Err(ErrorCode::CapacityExceeded)
    ));
    assert_eq!(authority.staged_clones.load(Ordering::Relaxed), 0);
    {
        let mut guard = authority.read().map_err(failure)?;
        assert_eq!(guard.publication(), 0);
        for lease in &leases {
            guard
                .check(lease, |view| {
                    assert_eq!(view.facts.tenants[&tenant.id].state, TenantState::Active);
                    Ok(())
                })
                .map_err(failure)?;
        }
    }
    // Failed preflight never installs a reservation that blocks subsequent work.
    drop(authority.prepare(initial, &[], None).map_err(failure)?);
    Ok(())
}

#[test]
fn mixed_expansion_then_shrink_uses_final_sum_at_full_budget() -> anyhow::Result<()> {
    let initial = header()?;
    let small = credential("c1", 1)?;
    let large = credential("c2", 256)?;
    let view = credential_view(&initial, &[small, large])?;
    let changes = [
        Change::Credential(credential("c1", 256)?),
        Change::Credential(credential("c2", 1)?),
    ];
    let predicted = forecast::bytes(&view, &changes, None)
        .map_err(failure)?
        .ok_or_else(|| anyhow::anyhow!("affected view"))?;
    assert!(predicted <= view.bytes);
    let (authority, leases) = full_authority(&initial, vec![view])?;
    authority
        .prepare(initial, &changes, None)
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    authority
        .read()
        .map_err(failure)?
        .check(&leases[0], |view| {
            assert!(view.bytes <= predicted);
            for (id, expected) in [("c1", 256), ("c2", 1)] {
                let key: bitrouter_ai::tenancy::identity::CredentialId =
                    id.try_into().map_err(|_| ErrorCode::AuthorityUnavailable)?;
                let GrantScope::Resources { resources } = &view.facts.credentials[&key].scope
                else {
                    return Err(ErrorCode::AuthorityUnavailable);
                };
                assert_eq!(resources.len(), expected);
            }
            Ok(())
        })
        .map_err(failure)?;
    Ok(())
}

#[test]
fn duplicate_intermediate_record_is_rejected_before_clone() -> anyhow::Result<()> {
    let initial = header()?;
    let small = credential("credential", 1)?;
    let view = credential_view(&initial, std::slice::from_ref(&small))?;
    let (authority, leases) = full_authority(&initial, vec![view])?;
    let changes = [
        Change::Credential(credential("credential", 256)?),
        Change::Credential(small),
    ];
    assert!(matches!(
        authority.prepare(initial.clone(), &changes, None),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    assert_eq!(authority.staged_clones.load(Ordering::Relaxed), 0);
    assert_eq!(authority.read().map_err(failure)?.publication(), 0);
    drop(authority.prepare(initial, &[], None).map_err(failure)?);
    drop(leases);
    Ok(())
}

#[test]
fn every_mutable_record_kind_has_a_distinct_duplicate_key() -> anyhow::Result<()> {
    let initial = header()?;
    let tenant = model::Tenant {
        id: "a".try_into()?,
        revision: "v1".try_into()?,
        state: TenantState::Active,
        created_at: initial.created_at.clone(),
    };
    let principal = model::Principal {
        id: "principal".try_into()?,
        revision: "v1".try_into()?,
        issuer: "issuer".into(),
        subject: "subject".into(),
        created_at: initial.created_at.clone(),
    };
    let binding = model::Binding {
        tenant: tenant.id.clone(),
        id: "binding".try_into()?,
        principal: principal.id.clone(),
        revision: "v1".try_into()?,
        role: TenantRole::Editor,
        scope: GrantScope::Tenant {
            operator_pools: Vec::new(),
        },
        expires: model::Deadline::parse("2030-01-01T00:00:00Z".try_into()?)?,
        managed_by: None,
    };
    let record = crate::tenancy::resources::lifecycle::Record {
        reference: reference()?,
        is_head: false,
        revoked: true,
        authority_revision: initial.revision,
    };
    let lifecycle = || Change::ResourceLifecycle {
        installation: initial.installation.clone(),
        record: record.clone(),
    };
    let tenant_id = tenant.id.clone();
    let revision = record.reference.revision_id.clone();
    let activation = || Change::Activation {
        tenant: tenant_id.clone(),
        expected: None,
        revision: revision.clone(),
        authority_revision: initial.revision,
    };
    for pair in [
        [Change::Tenant(tenant.clone()), Change::Tenant(tenant)],
        [
            Change::Principal(principal.clone()),
            Change::Principal(principal),
        ],
        [Change::Binding(binding.clone()), Change::Binding(binding)],
        [activation(), activation()],
        [lifecycle(), lifecycle()],
    ] {
        assert_eq!(
            forecast::unique(&pair),
            Err(ErrorCode::AuthorityUnavailable)
        );
    }
    Ok(())
}

#[test]
fn activation_insertion_forecasts_key_and_tree_growth() -> anyhow::Result<()> {
    let initial = header()?;
    let original = view(&initial, "a")?;
    let change = Change::Activation {
        tenant: "a".try_into()?,
        expected: None,
        revision: "active".try_into()?,
        authority_revision: initial.revision,
    };
    let changes = [change];
    let predicted = forecast::bytes(&original, &changes, None)
        .map_err(failure)?
        .ok_or_else(|| anyhow::anyhow!("affected view"))?;
    assert!(predicted > original.bytes);
    let mut constructed = original.clone();
    constructed.apply(&changes, None).map_err(failure)?;
    assert!(constructed.bytes <= predicted);
    let (authority, _leases) = full_authority(&initial, vec![original])?;
    assert!(matches!(
        authority.prepare(initial, &changes, None),
        Err(ErrorCode::CapacityExceeded)
    ));
    assert_eq!(authority.staged_clones.load(Ordering::Relaxed), 0);
    Ok(())
}
