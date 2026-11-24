use super::*;

fn authority(initial: &AuthorityHeader) -> anyhow::Result<Arc<Authority>> {
    Authority::new(
        initial.clone(),
        &AuthorityViews::default(),
        Arc::new(AtomicBool::new(true)),
    )
    .map_err(failure)
}

#[test]
fn held_invalid_leases_never_revive_through_replace_or_later_publication() -> anyhow::Result<()> {
    let initial = header()?;
    let authority = authority(&initial)?;
    let held = authority
        .register(view(&initial, "a")?, 0)
        .map_err(failure)?;
    let before = authority.read().map_err(failure)?.read_publication();
    authority.invalidate_retained_views().map_err(failure)?;
    assert_eq!(authority.integrity_epoch(), 1);
    assert_eq!(
        authority.read().map_err(failure)?.read_publication(),
        before + 1
    );
    assert!(matches!(
        authority.register(view(&initial, "b")?, before),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    assert_eq!(
        authority.replace(&held, view(&initial, "a")?, before + 1),
        Err(ErrorCode::AuthorityUnavailable)
    );
    let healthy = authority
        .register(view(&initial, "b")?, before + 1)
        .map_err(failure)?;
    let next = AuthorityHeader {
        revision: 2.try_into()?,
        ..initial
    };
    let tenant: TenantId = "a".try_into()?;
    let change = crate::tenancy::model::Tenant {
        state: TenantState::Suspended,
        ..view(&next, "a")?.facts.tenants[&tenant].clone()
    };
    authority
        .prepare(next.clone(), &[Change::Tenant(change)], None)
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    assert_eq!(
        authority.staged_clones.load(Ordering::Relaxed),
        0,
        "invalid held views stay charged without cloning or rebasing their facts"
    );
    assert_eq!(
        authority.integrity_epoch(),
        1,
        "ordinary publication must not advance the integrity epoch"
    );
    let called = AtomicBool::new(false);
    let mut guard = authority.read().map_err(failure)?;
    assert_eq!(
        guard.check(&held, |_| {
            called.store(true, Ordering::Release);
            Ok(())
        }),
        Err(ErrorCode::AuthorityUnavailable)
    );
    assert!(!called.load(Ordering::Acquire));
    guard
        .check(&healthy, |view| {
            assert_eq!(view.facts.revision, next.revision);
            Ok(())
        })
        .map_err(failure)?;
    drop(guard);
    assert_eq!(
        authority.replace(&held, view(&next, "a")?, before + 2),
        Err(ErrorCode::AuthorityUnavailable)
    );
    Ok(())
}

#[test]
fn invalid_owners_remain_charged_until_the_last_alias_drops() -> anyhow::Result<()> {
    for entry_limit in [true, false] {
        let initial = header()?;
        let value = view(&initial, "a")?;
        let payload = value.bytes;
        let budget = AuthorityViews {
            entries: if entry_limit { 1 } else { 8 },
            bytes: 3 * (memory::registry(2).map_err(failure)? + payload),
        };
        let authority = Authority::new(initial.clone(), &budget, Arc::new(AtomicBool::new(true)))
            .map_err(failure)?;
        let held = authority.register(value, 0).map_err(failure)?;
        let alias = held.clone();
        let (registry_capacity, registry_bytes) = {
            let state = authority
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("state"))?;
            (
                state.views.capacity(),
                state.registry_bytes().map_err(failure)?,
            )
        };
        authority.invalidate_retained_views().map_err(failure)?;
        {
            let state = authority
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("state"))?;
            assert_eq!(state.views.len(), 1);
            assert_eq!(state.views.capacity(), registry_capacity);
            assert_eq!(state.registry_bytes().map_err(failure)?, registry_bytes);
            assert_eq!(
                held.inner
                    .view
                    .lock()
                    .map_err(|_| anyhow::anyhow!("view"))?
                    .bytes,
                payload
            );
        }
        drop(held);
        assert!(matches!(
            authority.register(view(&initial, "b")?, 1),
            Err(ErrorCode::CapacityExceeded)
        ));
        drop(alias);
        let healthy = authority
            .register(view(&initial, "b")?, 1)
            .map_err(failure)?;
        authority
            .read()
            .map_err(failure)?
            .check(&healthy, |_| Ok(()))
            .map_err(failure)?;
    }
    Ok(())
}

#[test]
fn invalidation_serializes_with_final_checks_before_advancing_the_epoch() -> anyhow::Result<()> {
    let initial = header()?;
    let authority = authority(&initial)?;
    let held = authority
        .register(view(&initial, "a")?, 0)
        .map_err(failure)?;
    let mut guard = authority.read().map_err(failure)?;
    let (started, ready) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let writer = authority.clone();
    let worker = std::thread::spawn(move || -> Result<()> {
        started
            .send(())
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        done.send(writer.invalidate_retained_views())
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    });
    ready.recv_timeout(std::time::Duration::from_secs(5))?;
    guard.check(&held, |_| Ok(())).map_err(failure)?;
    assert_eq!(authority.integrity_epoch(), 0);
    assert!(matches!(
        finished.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    drop(guard);
    finished
        .recv_timeout(std::time::Duration::from_secs(5))?
        .map_err(failure)?;
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("invalidation worker failed"))?
        .map_err(failure)?;
    assert_eq!(authority.integrity_epoch(), 1);
    assert_eq!(
        authority.read().map_err(failure)?.check(&held, |_| Ok(())),
        Err(ErrorCode::AuthorityUnavailable)
    );
    Ok(())
}

#[test]
fn invalidation_preserves_prepared_healthy_publication_and_dropped_reservation()
-> anyhow::Result<()> {
    for publish in [false, true] {
        let initial = header()?;
        let authority = authority(&initial)?;
        let held = authority
            .register(view(&initial, "a")?, 0)
            .map_err(failure)?;
        let tenant: TenantId = "a".try_into()?;
        let change = crate::tenancy::model::Tenant {
            state: TenantState::Suspended,
            ..view(&initial, "a")?.facts.tenants[&tenant].clone()
        };
        let next = AuthorityHeader {
            revision: 2.try_into()?,
            ..initial.clone()
        };
        let prepared = authority
            .prepare(next.clone(), &[Change::Tenant(change)], None)
            .map_err(failure)?;
        authority.invalidate_retained_views().map_err(failure)?;
        authority.invalidate_retained_views().map_err(failure)?;
        {
            let state = authority
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("state"))?;
            assert!(state.preparing);
            assert_eq!(
                state.publication, 0,
                "detection must preserve the writer reservation"
            );
            assert_eq!(state.read_publication, 2);
        }
        assert!(matches!(
            authority.register(view(&initial, "b")?, 2),
            Err(ErrorCode::AuthorityUnavailable)
        ));
        if publish {
            prepared.publish().map_err(failure)?;
        } else {
            drop(prepared);
        }
        let expected = if publish { 3 } else { 2 };
        let current = if publish { &next } else { &initial };
        {
            let state = authority
                .state
                .read()
                .map_err(|_| anyhow::anyhow!("state"))?;
            assert!(!state.preparing);
            assert_eq!(state.publication, u64::from(publish));
            assert_eq!(state.read_publication, expected);
        }
        assert!(matches!(
            authority.register(view(current, "b")?, 0),
            Err(ErrorCode::AuthorityUnavailable)
        ));
        let healthy = authority
            .register(view(current, "b")?, expected)
            .map_err(failure)?;
        let mut guard = authority.read().map_err(failure)?;
        assert_eq!(
            guard.check(&held, |_| Ok(())),
            Err(ErrorCode::AuthorityUnavailable)
        );
        guard.check(&healthy, |_| Ok(())).map_err(failure)?;
        assert_eq!(guard.header().revision, current.revision);
        assert_eq!(authority.integrity_epoch(), 2);
    }
    Ok(())
}

#[test]
fn exhausted_counters_refuse_invalidation_without_partial_changes() -> anyhow::Result<()> {
    for epoch_overflow in [true, false] {
        let initial = header()?;
        let authority = authority(&initial)?;
        let held = authority
            .register(view(&initial, "a")?, 0)
            .map_err(failure)?;
        {
            let mut state = authority
                .state
                .write()
                .map_err(|_| anyhow::anyhow!("state"))?;
            if epoch_overflow {
                authority.integrity_epoch.store(u64::MAX, Ordering::Release);
            } else {
                state.read_publication = u64::MAX;
                held.inner
                    .view
                    .lock()
                    .map_err(|_| anyhow::anyhow!("view"))?
                    .publication = u64::MAX;
            }
        }
        assert_eq!(
            authority.invalidate_retained_views(),
            Err(ErrorCode::AuthorityUnavailable)
        );
        assert!(!held.inner.invalidated.load(Ordering::Acquire));
        assert_eq!(
            authority.integrity_epoch(),
            if epoch_overflow { u64::MAX } else { 0 }
        );
        let mut guard = authority.read().map_err(failure)?;
        assert_eq!(
            guard.read_publication(),
            if epoch_overflow { 0 } else { u64::MAX }
        );
        guard.check(&held, |_| Ok(())).map_err(failure)?;
    }
    Ok(())
}
