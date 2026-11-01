use super::*;
use crate::tenancy::oauth::{profile::Profile, storage};
use std::sync::atomic::Ordering;

fn metadata(initial: &AuthorityHeader) -> anyhow::Result<Metadata> {
    let reference = ResourceRef {
        resource_kind: ResourceKind::Credential,
        resource_id: "slot".try_into()?,
        ..reference()?
    };
    let registration = crate::gateway::providers::login::find("anthropic")
        .ok_or_else(|| anyhow::anyhow!("registration"))?;
    let value = Metadata {
        format: 1,
        installation: initial.installation.clone(),
        slot: storage::slot(&initial.installation, &reference)?,
        reference,
        profile: Profile {
            format: 1,
            mechanism: "claude_code_refresh_v1".into(),
            provider: "claude-code".into(),
            account: "default".into(),
            registration: "anthropic".into(),
            token_endpoint: registration.auth.token_endpoint,
            client_id: registration.auth.client_id,
        },
        seed: "a".repeat(64),
        generation: 0,
        state: storage::State::Ready,
    };
    value.validate()?;
    Ok(value)
}

fn oauth_view(
    initial: &AuthorityHeader,
    reference: &ResourceRef,
    value: Option<&Metadata>,
) -> anyhow::Result<View> {
    View::new(
        view(initial, "a")?.facts,
        vec![OAuthFact {
            reference: reference.clone(),
            slot: storage::slot(&initial.installation, reference)?,
            metadata: value.cloned(),
        }],
    )
    .map_err(failure)
}

#[test]
fn full_budget_oauth_patches_positive_negative_and_conflicting_revision_views() -> anyhow::Result<()>
{
    let initial = header()?;
    let old = metadata(&initial)?;
    let other_revision = ResourceRef {
        revision_id: "v2".try_into()?,
        ..old.reference.clone()
    };
    let (authority, leases) = super::capacity::full_authority(
        &initial,
        vec![
            oauth_view(&initial, &old.reference, Some(&old))?,
            oauth_view(&initial, &old.reference, None)?,
            oauth_view(&initial, &other_revision, Some(&old))?,
        ],
    )?;
    {
        let mut guard = authority.read().map_err(failure)?;
        guard
            .check(&leases[0], |view| {
                assert!(view.oauth(&old.reference)? == Some(&old));
                Ok(())
            })
            .map_err(failure)?;
        guard
            .check(&leases[1], |view| {
                assert!(view.oauth(&old.reference)?.is_none());
                let mut not_loaded = old.reference.clone();
                not_loaded.resource_id = "other-slot"
                    .try_into()
                    .map_err(|_| ErrorCode::AuthorityUnavailable)?;
                assert_eq!(
                    view.oauth(&not_loaded).err(),
                    Some(ErrorCode::AuthorityUnavailable)
                );
                Ok(())
            })
            .map_err(failure)?;
        guard
            .check(&leases[2], |view| {
                assert_eq!(
                    view.oauth(&other_revision).err(),
                    Some(ErrorCode::CapabilityMismatch)
                );
                Ok(())
            })
            .map_err(failure)?;
    }
    let mut started = old.clone();
    started.state = storage::State::RefreshStarted(storage::Work {
        id: "00000000-0000-4000-8000-000000000001".into(),
        execution: "execution".try_into()?,
        tenant: "a".try_into()?,
        principal: "principal".try_into()?,
        expected_generation: 0,
        started_at: "2026-01-01T00:00:00Z".try_into()?,
    });
    started.validate()?;
    authority
        .prepare(initial.clone(), &[], Some((&old, &started)))
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    {
        let mut guard = authority.read().map_err(failure)?;
        assert_eq!(guard.publication(), 1);
        assert_eq!(guard.header().revision, initial.revision);
        for lease in leases.iter().take(2) {
            guard
                .check(lease, |view| {
                    assert!(view.oauth(&old.reference)? == Some(&started));
                    Ok(())
                })
                .map_err(failure)?;
        }
    }
    let mut refreshed = started.clone();
    refreshed.generation = 1;
    refreshed.state = storage::State::Ready;
    refreshed.validate()?;
    authority
        .prepare(initial.clone(), &[], Some((&started, &refreshed)))
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    let mut guard = authority.read().map_err(failure)?;
    assert_eq!(guard.publication(), 2);
    assert_eq!(guard.header().revision, initial.revision);
    assert_eq!(authority.staged_clones.load(Ordering::Relaxed), 6);
    for lease in leases.iter().take(2) {
        guard
            .check(lease, |view| {
                assert!(view.oauth(&old.reference)? == Some(&refreshed));
                assert!(view.oauth(&old.reference)? != Some(&old));
                assert_eq!(view.facts.revision, initial.revision);
                Ok(())
            })
            .map_err(failure)?;
    }
    guard
        .check(&leases[2], |view| {
            assert_eq!(
                view.oauth(&other_revision).err(),
                Some(ErrorCode::CapabilityMismatch)
            );
            Ok(())
        })
        .map_err(failure)?;
    Ok(())
}

#[test]
fn stale_expected_oauth_metadata_rejects_before_cloning_earlier_negative_view() -> anyhow::Result<()>
{
    let initial = header()?;
    let old = metadata(&initial)?;
    let (authority, leases) = super::capacity::full_authority(
        &initial,
        vec![
            oauth_view(&initial, &old.reference, None)?,
            oauth_view(&initial, &old.reference, Some(&old))?,
        ],
    )?;
    let mut stale = old.clone();
    stale.generation = 7;
    let mut next = stale.clone();
    next.generation = 8;
    assert!(matches!(
        authority.prepare(initial.clone(), &[], Some((&stale, &next))),
        Err(ErrorCode::AuthorityUnavailable)
    ));
    assert_eq!(authority.staged_clones.load(Ordering::Relaxed), 0);
    {
        let mut guard = authority.read().map_err(failure)?;
        assert_eq!(guard.publication(), 0);
        guard
            .check(&leases[0], |view| {
                assert!(view.oauth(&old.reference)?.is_none());
                Ok(())
            })
            .map_err(failure)?;
        guard
            .check(&leases[1], |view| {
                assert!(view.oauth(&old.reference)? == Some(&old));
                Ok(())
            })
            .map_err(failure)?;
    }
    next.generation = 1;
    authority
        .prepare(initial, &[], Some((&old, &next)))
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    Ok(())
}
