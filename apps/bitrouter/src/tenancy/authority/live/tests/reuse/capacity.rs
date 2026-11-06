use super::*;

#[test]
fn one_entry_retention_cannot_refuse_the_next_cold_request_or_evict_an_owner() -> anyhow::Result<()>
{
    for hit_first in [false, true] {
        let initial = header()?;
        let budget = AuthorityViews {
            entries: 1,
            ..AuthorityViews::default()
        };
        let authority = Authority::new(initial.clone(), &budget, Arc::new(AtomicBool::new(true)))
            .map_err(failure)?;
        let lease = authority
            .register(prepared(&initial, &[])?, 0)
            .map_err(failure)?;
        authority
            .retain_prepared(&caller()?, &lease, &[])
            .map_err(failure)?;
        drop(lease);
        if hit_first {
            assert!(
                authority
                    .try_reuse_token(&hash())
                    .map_err(failure)?
                    .is_none()
            );
        }
        let active = authority
            .register(prepared(&initial, &[])?, 0)
            .map_err(failure)?;
        authority
            .retain_prepared(&caller()?, &active, &[])
            .map_err(failure)?;
        assert!(
            authority
                .try_reuse_token(&hash())
                .map_err(failure)?
                .is_none()
        );
        assert!(matches!(
            authority.register(prepared(&initial, &[])?, 0),
            Err(ErrorCode::CapacityExceeded)
        ));
        authority
            .read()
            .map_err(failure)?
            .check(&active, |_| Ok(()))
            .map_err(failure)?;
    }
    Ok(())
}

#[test]
fn payload_is_charged_once_but_copy_and_container_capacity_are_charged() -> anyhow::Result<()> {
    let initial = header()?;
    let value = prepared(&initial, &[])?;
    let working =
        memory::registry(1).map_err(failure)? + value.bytes + memory::reuse(1).map_err(failure)?;
    let budget = AuthorityViews {
        entries: 8,
        bytes: 3 * working,
    };
    let authority = Authority::new(initial.clone(), &budget, Arc::new(AtomicBool::new(true)))
        .map_err(failure)?;
    let lease = authority.register(value, 0).map_err(failure)?;
    let alias = lease.clone();
    authority
        .retain_prepared(&caller()?, &lease, &[])
        .map_err(failure)?;
    {
        let state = authority
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("state"))?;
        assert_eq!(state.views.len(), 1);
        assert_eq!(state.reuse.len(), 1);
        assert!(
            state.registry_bytes().map_err(failure)?
                + lease
                    .inner
                    .view
                    .lock()
                    .map_err(|_| anyhow::anyhow!("view"))?
                    .bytes
                <= working
        );
    }
    drop((lease, alias));
    assert!(
        authority
            .try_reuse_token(&hash())
            .map_err(failure)?
            .is_none()
    );
    let cold = authority
        .register(prepared(&initial, &[])?, 0)
        .map_err(failure)?;
    // At exactly the original cold budget optional insertion silently declines.
    let (full, leases) =
        super::super::capacity::full_authority(&initial, vec![prepared(&initial, &[])?])?;
    full.retain_prepared(&caller()?, &leases[0], &[])
        .map_err(failure)?;
    assert!(full.try_reuse_token(&hash()).map_err(failure)?.is_none());
    drop(cold);
    Ok(())
}

#[test]
fn growing_replace_and_publication_drop_only_soft_owners_before_preflight() -> anyhow::Result<()> {
    for publish in [false, true] {
        for hold_source in [false, true] {
            let initial = header()?;
            let small = prepared(&initial, &[])?;
            let mut growing = prepared(&initial, &[])?;
            for principal in growing.facts.principals.values_mut() {
                principal.issuer = "g".repeat(256);
            }
            let large = View::new(growing.facts, Vec::new()).map_err(failure)?;
            let working = memory::registry(2).map_err(failure)?
                + 2 * small.bytes
                + 2 * memory::reuse(1).map_err(failure)?;
            assert!(large.bytes > small.bytes + 2 * memory::reuse(1).map_err(failure)?);
            assert!(large.bytes + memory::registry(2).map_err(failure)? < working);
            let budget = AuthorityViews {
                entries: 2,
                bytes: 3 * working,
            };
            let authority =
                Authority::new(initial.clone(), &budget, Arc::new(AtomicBool::new(true)))
                    .map_err(failure)?;
            let soft = authority.register(small, 0).map_err(failure)?;
            let owner = authority
                .register(prepared(&initial, &[])?, 0)
                .map_err(failure)?;
            authority
                .retain_prepared(&caller()?, &soft, &[])
                .map_err(failure)?;
            let held = hold_source.then_some(soft.clone());
            drop(soft);
            let result = if publish {
                let change = large
                    .facts
                    .principals
                    .values()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("principal"))?
                    .clone();
                authority
                    .prepare(initial, &[Change::Principal(change)], None)
                    .and_then(|publication| publication.publish())
            } else {
                authority.replace(&owner, large, 0)
            };
            if hold_source {
                assert_eq!(result, Err(ErrorCode::CapacityExceeded));
                assert_eq!(authority.staged_clones.load(Ordering::Relaxed), 0);
            } else {
                result.map_err(failure)?;
            }
            authority
                .read()
                .map_err(failure)?
                .check(&owner, |_| Ok(()))
                .map_err(failure)?;
            assert!(
                authority
                    .try_reuse_token(&hash())
                    .map_err(failure)?
                    .is_none()
            );
            drop(held);
        }
    }
    Ok(())
}

#[test]
fn eviction_releases_peak_registry_before_a_cold_fitting_large_grant() -> anyhow::Result<()> {
    use bitrouter_ai::tenancy::{authority::ResourceKey, validation::Validate};
    for keep_active in [false, true] {
        let initial = header()?;
        let mut facts = prepared(&initial, &[])?.facts;
        let scope = GrantScope::Resources {
            resources: (0..256)
                .map(|index| -> anyhow::Result<ResourceKey> {
                    Ok(ResourceKey {
                        owner_kind: OwnerKind::Tenant,
                        owner_id: "a".try_into()?,
                        resource_kind: ResourceKind::Route,
                        resource_id: format!("{index:04}{}", "r".repeat(124)).try_into()?,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?,
        };
        scope.validate()?;
        for credential in facts.credentials.values_mut() {
            credential.scope = scope.clone();
        }
        for binding in facts.bindings.values_mut() {
            binding.scope = scope.clone();
        }
        let large = View::new(facts, Vec::new()).map_err(failure)?;
        let mut smalls = Vec::new();
        for index in 0..3 {
            let mut facts = prepared(&initial, &[])?.facts;
            let (_, mut credential) = facts
                .credentials
                .pop_first()
                .ok_or_else(|| anyhow::anyhow!("credential"))?;
            credential.id = format!("small-{index}").try_into()?;
            credential.key_hash = crate::auth::keys::hash_key(&format!("brtk_small_{index}"));
            let caller = Caller::Credential(credential.id.clone());
            facts.credentials.insert(credential.id.clone(), credential);
            smalls.push((caller, View::new(facts, Vec::new()).map_err(failure)?));
        }
        let active_bytes = if keep_active { smalls[0].1.bytes } else { 0 };
        let entries = 1 + usize::from(keep_active);
        let working = large.bytes + active_bytes + memory::registry(entries).map_err(failure)?;
        let small_total = smalls.iter().map(|(_, view)| view.bytes).sum::<usize>();
        assert!(
            small_total
                + memory::registry(3).map_err(failure)?
                + 2 * memory::reuse(3).map_err(failure)?
                < working
        );
        assert!(large.bytes + active_bytes + memory::registry(3).map_err(failure)? > working);
        let budget = AuthorityViews {
            entries: 4,
            bytes: 3 * working,
        };
        let authority =
            Authority::new(initial, &budget, Arc::new(AtomicBool::new(true))).map_err(failure)?;
        let mut active = None;
        for (index, (caller, view)) in smalls.into_iter().enumerate() {
            let lease = authority.register(view, 0).map_err(failure)?;
            authority
                .retain_prepared(&caller, &lease, &[])
                .map_err(failure)?;
            if keep_active && index == 0 {
                active = Some(lease);
            }
        }
        {
            let state = authority
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("state"))?;
            assert_eq!(state.reuse.len(), 3);
            assert!(state.views.capacity() >= 3);
        }
        let admitted = authority.register(large, 0).map_err(failure)?;
        {
            let state = authority
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("state"))?;
            assert!(state.reuse.is_empty());
            assert_eq!(state.reuse.capacity(), 0);
            assert_eq!(state.views.len(), entries);
            assert_eq!(state.views.capacity(), entries);
        }
        if let Some(active) = active {
            authority
                .read()
                .map_err(failure)?
                .check(&active, |_| Ok(()))
                .map_err(failure)?;
        }
        authority
            .read()
            .map_err(failure)?
            .check(&admitted, |_| Ok(()))
            .map_err(failure)?;
    }
    Ok(())
}
