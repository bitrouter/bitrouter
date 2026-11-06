use super::*;
mod capacity;
mod oauth;
mod reuse;
use crate::config::tenancy::capacity::AuthorityViews;
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind, TenantState},
    identity::TenantId,
};

fn header() -> anyhow::Result<AuthorityHeader> {
    Ok(AuthorityHeader {
        installation: "installation".try_into()?,
        revision: 1.try_into()?,
        operation_ids: 0,
        safety_operation_ids: 0,
        saturated: false,
        created_at: "2026-01-01T00:00:00Z".into(),
    })
}

fn view(header: &AuthorityHeader, tenant: &str) -> anyhow::Result<View> {
    let mut facts = Snapshot::from_header(header);
    let tenant = crate::tenancy::model::Tenant {
        id: tenant.try_into()?,
        revision: "v1".try_into()?,
        state: TenantState::Active,
        created_at: header.created_at.clone(),
    };
    facts.tenants.insert(tenant.id.clone(), tenant);
    View::new(facts, Vec::new()).map_err(failure)
}

fn failure(code: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("authority view: {code:?}")
}

fn reference() -> anyhow::Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "a".try_into()?,
        resource_kind: ResourceKind::Route,
        resource_id: "route".try_into()?,
        revision_id: "v1".try_into()?,
    })
}

#[test]
fn quarantine_denies_an_existing_guard_without_a_publication_change() -> anyhow::Result<()> {
    let initial = header()?;
    let live = Arc::new(AtomicBool::new(true));
    let proof = crate::tenancy::transaction::Proof::new(live.clone());
    let authority =
        Authority::new(initial.clone(), &AuthorityViews::default(), live).map_err(failure)?;
    let lease = authority
        .register(view(&initial, "a")?, 0)
        .map_err(failure)?;
    let mut guard = authority.read().map_err(failure)?;
    guard.check(&lease, |_| Ok(())).map_err(failure)?;
    proof.quarantine();
    assert_eq!(guard.publication(), 0);
    assert_eq!(guard.header().revision, initial.revision);
    assert!(matches!(
        guard.check(&lease, |_| Ok(())),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    drop(guard);
    // Cleanup can still fence caches and inspect immutable origin identity.
    authority.with_write(|| Ok(())).map_err(failure)?;
    assert_eq!(
        authority.read().map_err(failure)?.header().installation,
        initial.installation
    );
    Ok(())
}

#[test]
fn unrelated_publication_rebases_live_views_and_related_change_is_current() -> anyhow::Result<()> {
    let initial = header()?;
    let authority = Authority::new(
        initial.clone(),
        &AuthorityViews::default(),
        Arc::new(AtomicBool::new(true)),
    )
    .map_err(failure)?;
    let a = authority
        .register(view(&initial, "a")?, 0)
        .map_err(failure)?;
    let b = authority
        .register(view(&initial, "b")?, 0)
        .map_err(failure)?;
    let tenant_b: TenantId = "b".try_into()?;
    let mut next = initial.clone();
    next.revision = 2.try_into()?;
    let change = crate::tenancy::model::Tenant {
        id: tenant_b.clone(),
        revision: "v2".try_into()?,
        state: TenantState::Suspended,
        created_at: initial.created_at.clone(),
    };
    authority
        .prepare(next, &[Change::Tenant(change)], None)
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    let mut guard = authority.read().map_err(failure)?;
    guard
        .check(&a, |view| {
            assert_eq!(view.facts.revision.get(), 2);
            Ok(())
        })
        .map_err(failure)?;
    guard
        .check(&b, |view| {
            assert_eq!(view.facts.tenants[&tenant_b].state, TenantState::Suspended);
            Ok(())
        })
        .map_err(failure)?;
    // Repeated/aliased owner checks are sequential beneath one publication lock.
    guard.check(&a.clone(), |_| Ok(())).map_err(failure)?;
    Ok(())
}

#[test]
fn full_view_budget_still_publishes_exact_revocation_for_loaded_absence() -> anyhow::Result<()> {
    let initial = header()?;
    let reference = reference()?;
    let mut facts = view(&initial, "a")?.facts;
    facts.resource_lifecycle.absent(reference.clone());
    let view = View::new(facts, Vec::new()).map_err(failure)?;
    let budget = AuthorityViews {
        entries: 1,
        bytes: 3 * (memory::registry(1).map_err(failure)? + view.bytes),
    };
    let authority = Authority::new(initial.clone(), &budget, Arc::new(AtomicBool::new(true)))
        .map_err(failure)?;
    let lease = authority.register(view, 0).map_err(failure)?;
    let mut next = initial.clone();
    next.revision = 2.try_into()?;
    let record = crate::tenancy::resources::lifecycle::Record {
        reference: reference.clone(),
        is_head: false,
        revoked: true,
        authority_revision: next.revision,
    };
    authority
        .prepare(
            next,
            &[Change::ResourceLifecycle {
                installation: initial.installation.clone(),
                record,
            }],
            None,
        )
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    authority
        .read()
        .map_err(failure)?
        .check(&lease, |view| {
            assert!(view.facts.resource_lifecycle.revoked(&reference));
            Ok(())
        })
        .map_err(failure)?;
    Ok(())
}

#[test]
fn unloaded_resources_deny_and_dropped_owners_release_capacity() -> anyhow::Result<()> {
    let initial = header()?;
    let budget = AuthorityViews {
        entries: 1,
        ..AuthorityViews::default()
    };
    let authority = Authority::new(initial.clone(), &budget, Arc::new(AtomicBool::new(true)))
        .map_err(failure)?;
    let lease = authority
        .register(view(&initial, "a")?, 0)
        .map_err(failure)?;
    let reference = reference()?;
    authority
        .read()
        .map_err(failure)?
        .check(&lease, |view| {
            assert!(view.facts.resource_lifecycle.revoked(&reference));
            assert_eq!(
                view.oauth(&reference).err(),
                Some(ErrorCode::AuthorityUnavailable)
            );
            Ok(())
        })
        .map_err(failure)?;
    assert!(matches!(
        authority.register(view(&initial, "b")?, 0),
        Err(ErrorCode::CapacityExceeded)
    ));
    drop(lease);
    authority
        .register(view(&initial, "b")?, 0)
        .map_err(failure)?;
    Ok(())
}

#[test]
fn discarded_preparation_preserves_authority_and_stale_reads_cannot_register() -> anyhow::Result<()>
{
    let initial = header()?;
    let authority = Authority::new(
        initial.clone(),
        &AuthorityViews::default(),
        Arc::new(AtomicBool::new(true)),
    )
    .map_err(failure)?;
    let lease = authority
        .register(view(&initial, "a")?, 0)
        .map_err(failure)?;
    let prepared = authority
        .prepare(initial.clone(), &[], None)
        .map_err(failure)?;
    assert!(matches!(
        authority.register(view(&initial, "b")?, 0),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    drop(prepared);
    assert_eq!(authority.read().map_err(failure)?.publication(), 0);
    authority
        .prepare(initial.clone(), &[], None)
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    assert!(matches!(
        authority.register(view(&initial, "b")?, 0),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    assert!(matches!(
        authority.replace(&lease, view(&initial, "a")?, 0),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    authority
        .replace(&lease, view(&initial, "a")?, 1)
        .map_err(failure)?;
    Ok(())
}

#[test]
fn a_waiting_publisher_cannot_split_related_owner_checks() -> anyhow::Result<()> {
    let initial = header()?;
    let authority = Authority::new(
        initial.clone(),
        &AuthorityViews::default(),
        Arc::new(AtomicBool::new(true)),
    )
    .map_err(failure)?;
    let lease = authority
        .register(view(&initial, "a")?, 0)
        .map_err(failure)?;
    let next = AuthorityHeader {
        revision: 2.try_into()?,
        ..initial
    };
    let prepared = authority.prepare(next, &[], None).map_err(failure)?;
    let mut guard = authority.read().map_err(failure)?;
    let (started, ready) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let publisher = std::thread::spawn(move || {
        let _ = started.send(());
        let result = prepared.publish();
        let _ = done.send(result);
    });
    ready.recv()?;
    guard
        .check(&lease, |view| {
            assert_eq!(view.facts.revision.get(), 1);
            Ok(())
        })
        .map_err(failure)?;
    guard
        .check(&lease, |view| {
            assert_eq!(view.facts.revision.get(), 1);
            Ok(())
        })
        .map_err(failure)?;
    assert!(matches!(
        finished.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    drop(guard);
    finished
        .recv_timeout(std::time::Duration::from_secs(5))?
        .map_err(failure)?;
    publisher
        .join()
        .map_err(|_| anyhow::anyhow!("publisher thread failed"))?;
    authority
        .read()
        .map_err(failure)?
        .check(&lease, |view| {
            assert_eq!(view.facts.revision.get(), 2);
            Ok(())
        })
        .map_err(failure)?;
    Ok(())
}

#[test]
fn closing_fence_uses_common_gate_without_advancing_publication() -> anyhow::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    let initial = header()?;
    let authority = Authority::new(
        initial.clone(),
        &AuthorityViews::default(),
        Arc::new(AtomicBool::new(true)),
    )
    .map_err(failure)?;
    let lease = authority
        .register(view(&initial, "a")?, 0)
        .map_err(failure)?;
    let closed = Arc::new(AtomicBool::new(false));
    let mut guard = authority.read().map_err(failure)?;
    let (started, ready) = std::sync::mpsc::channel();
    let (done, finished) = std::sync::mpsc::channel();
    let writer = authority.clone();
    let flag = closed.clone();
    let closer = std::thread::spawn(move || -> Result<()> {
        started
            .send(())
            .map_err(|_| ErrorCode::AuthorityUnavailable)?;
        let result = writer.with_write(|| {
            flag.store(true, Ordering::Release);
            Ok(())
        });
        done.send(result)
            .map_err(|_| ErrorCode::AuthorityUnavailable)
    });
    ready.recv_timeout(Duration::from_secs(5))?;
    guard
        .check(&lease, |_| {
            assert!(!closed.load(Ordering::Acquire));
            Ok(())
        })
        .map_err(failure)?;
    assert!(matches!(
        finished.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));
    drop(guard);
    finished
        .recv_timeout(Duration::from_secs(5))?
        .map_err(failure)?;
    closer
        .join()
        .map_err(|_| anyhow::anyhow!("closing thread failed"))?
        .map_err(failure)?;
    let guard = authority.read().map_err(failure)?;
    assert!(closed.load(Ordering::Acquire));
    assert_eq!(guard.publication(), 0);
    assert_eq!(guard.header().revision, initial.revision);
    Ok(())
}
