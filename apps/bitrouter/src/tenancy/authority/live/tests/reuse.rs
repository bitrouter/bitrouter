use super::*;
use crate::tenancy::model::{self, Caller};
use bitrouter_ai::tenancy::authority::{CredentialState, GrantScope, Permission, TenantRole};

mod capacity;

fn caller() -> anyhow::Result<Caller> {
    Ok(Caller::Credential("credential".try_into()?))
}
fn hash() -> String {
    crate::auth::keys::hash_key("brtk_reuse_fixture")
}

fn prepared(initial: &AuthorityHeader, references: &[ResourceRef]) -> anyhow::Result<View> {
    let mut facts = view(initial, "a")?.facts;
    let principal = model::Principal {
        id: "principal".try_into()?,
        revision: "v1".try_into()?,
        issuer: "fixture".into(),
        subject: "subject".into(),
        created_at: initial.created_at.clone(),
    };
    let binding = model::Binding {
        tenant: "a".try_into()?,
        id: "binding".try_into()?,
        principal: principal.id.clone(),
        revision: "v1".try_into()?,
        role: TenantRole::Editor,
        scope: GrantScope::Tenant {
            operator_pools: Vec::new(),
        },
        expires: model::Deadline::parse("2099-01-01T00:00:00Z".try_into()?)?,
        managed_by: None,
    };
    let credential = model::Credential {
        id: "credential".try_into()?,
        tenant: binding.tenant.clone(),
        principal: principal.id.clone(),
        binding: Some(binding.id.clone()),
        revision: "v1".try_into()?,
        key_hash: hash(),
        state: CredentialState::Active,
        audit_reader: false,
        permissions: vec![Permission::ExecutionSubmit],
        scope: binding.scope.clone(),
        expires: binding.expires.clone(),
        created_at: initial.created_at.clone(),
    };
    facts.principals.insert(principal.id.clone(), principal);
    facts
        .bindings
        .insert((binding.tenant.clone(), binding.id.clone()), binding);
    facts.credentials.insert(credential.id.clone(), credential);
    let mut oauth = Vec::new();
    for reference in references {
        facts.resource_lifecycle.absent(reference.clone());
        if reference.resource_kind == ResourceKind::Credential {
            oauth.push(OAuthFact {
                reference: reference.clone(),
                slot: crate::tenancy::oauth::storage::slot(&initial.installation, reference)?,
                metadata: None,
            });
        }
    }
    View::new(facts, oauth).map_err(failure)
}

fn authority(initial: &AuthorityHeader) -> anyhow::Result<Arc<Authority>> {
    Authority::new(
        initial.clone(),
        &AuthorityViews::default(),
        Arc::new(AtomicBool::new(true)),
    )
    .map_err(failure)
}

fn reused(authority: &Arc<Authority>) -> anyhow::Result<Authenticated> {
    authority
        .try_reuse_token(&hash())
        .map_err(failure)?
        .ok_or_else(|| anyhow::anyhow!("expected reuse"))
}

#[test]
fn separate_requests_exact_sets_and_owner_growth_do_not_share_a_template() -> anyhow::Result<()> {
    let initial = header()?;
    let authority = authority(&initial)?;
    let a = reference()?;
    let b = ResourceRef {
        resource_id: "other".try_into()?,
        ..a.clone()
    };
    let source = authority
        .register(prepared(&initial, std::slice::from_ref(&a))?, 0)
        .map_err(failure)?;
    authority
        .retain_prepared(&caller()?, &source, &[a.clone(), a.clone()])
        .map_err(failure)?;
    let request = reused(&authority)?;
    assert!(!Arc::ptr_eq(&source.inner, &request.lease.inner));
    assert!(matches!(request.caller, Caller::Credential(_)));
    {
        let mut guard = authority.read().map_err(failure)?;
        guard
            .check(&request.lease, |view| {
                assert!(view.covers(&[a.clone(), a.clone()], DependencyIntent::Fresh)?);
                assert!(!view.covers(std::slice::from_ref(&b), DependencyIntent::Fresh)?);
                Ok(())
            })
            .map_err(failure)?;
    }
    authority
        .replace(
            &request.lease,
            prepared(&initial, std::slice::from_ref(&b))?,
            0,
        )
        .map_err(failure)?;
    authority
        .read()
        .map_err(failure)?
        .check(&source, |view| {
            assert!(view.covers(std::slice::from_ref(&a), DependencyIntent::Fresh)?);
            assert!(!view.facts.resource_lifecycle.loaded(&b));
            Ok(())
        })
        .map_err(failure)?;
    // Growing the retained source itself must first remove its soft alias.
    authority
        .replace(&source, prepared(&initial, &[a.clone(), b.clone()])?, 0)
        .map_err(failure)?;
    authority
        .retain_prepared(&caller()?, &source, std::slice::from_ref(&b))
        .map_err(failure)?;
    assert!(
        authority
            .try_reuse_token(&hash())
            .map_err(failure)?
            .is_none()
    );
    authority
        .read()
        .map_err(failure)?
        .check(&source, |view| {
            assert!(view.covers(std::slice::from_ref(&b), DependencyIntent::Owner)?);
            assert!(!view.covers(std::slice::from_ref(&b), DependencyIntent::Fresh)?);
            Ok(())
        })
        .map_err(failure)?;
    Ok(())
}

#[test]
fn retained_and_copied_views_observe_denial_and_expiry_without_header_shortcuts()
-> anyhow::Result<()> {
    for denial in ["credential", "binding", "tenant", "lifecycle", "expiry"] {
        let initial = header()?;
        let authority = authority(&initial)?;
        let reference = reference()?;
        let mut source_view = prepared(&initial, std::slice::from_ref(&reference))?;
        if denial == "expiry" {
            let expires = chrono::Utc::now() + chrono::Duration::seconds(1);
            for credential in source_view.facts.credentials.values_mut() {
                credential.expires = model::Deadline::parse(
                    expires
                        .to_rfc3339_opts(chrono::SecondsFormat::Nanos, true)
                        .try_into()?,
                )?;
            }
            source_view = View::new(source_view.facts, source_view.oauth).map_err(failure)?;
        }
        let mut credential = source_view
            .facts
            .credentials
            .values()
            .next()
            .ok_or_else(|| anyhow::anyhow!("credential"))?
            .clone();
        let mut binding = source_view
            .facts
            .bindings
            .values()
            .next()
            .ok_or_else(|| anyhow::anyhow!("binding"))?
            .clone();
        let mut tenant = source_view
            .facts
            .tenants
            .values()
            .next()
            .ok_or_else(|| anyhow::anyhow!("tenant"))?
            .clone();
        let source = authority.register(source_view, 0).map_err(failure)?;
        authority
            .retain_prepared(&caller()?, &source, std::slice::from_ref(&reference))
            .map_err(failure)?;
        let copy = reused(&authority)?;
        let change = match denial {
            "credential" => {
                credential.state = CredentialState::Revoked;
                Change::Credential(credential)
            }
            "binding" => {
                binding.expires = model::Deadline::parse("2000-01-01T00:00:00Z".try_into()?)?;
                Change::Binding(binding)
            }
            "tenant" => {
                tenant.state = TenantState::Suspended;
                Change::Tenant(tenant)
            }
            "lifecycle" => Change::ResourceLifecycle {
                installation: initial.installation.clone(),
                record: crate::tenancy::resources::lifecycle::Record {
                    reference: reference.clone(),
                    is_head: false,
                    revoked: true,
                    authority_revision: initial.revision,
                },
            },
            _ => {
                // Cross the real credential deadline without changing facts or
                // publication. Both the retained source and held copy must deny.
                if let Ok(remaining) = (credential.expires.instant - chrono::Utc::now()).to_std() {
                    std::thread::sleep(remaining + std::time::Duration::from_millis(1));
                }
                assert_eq!(authority.read().map_err(failure)?.publication(), 0);
                assert!(matches!(
                    authority.try_reuse_token(&hash()),
                    Err(ErrorCode::Unauthorized)
                ));
                for lease in [&source, &copy.lease] {
                    assert!(matches!(
                        authority.read().map_err(failure)?.check(lease, |view| view
                            .facts
                            .credential(
                                &caller().map_err(|_| ErrorCode::AuthorityUnavailable)?,
                                chrono::Utc::now()
                            )
                            .map(|_| ())),
                        Err(ErrorCode::Unauthorized)
                    ));
                }
                continue;
            }
        };
        authority
            .prepare(initial.clone(), &[change], None)
            .map_err(failure)?
            .publish()
            .map_err(failure)?;
        let mut guard = authority.read().map_err(failure)?;
        assert_eq!(guard.header().revision, initial.revision);
        for lease in [&source, &copy.lease] {
            guard
                .check(lease, |view| {
                    if denial == "lifecycle" {
                        assert!(view.facts.resource_lifecycle.revoked(&reference));
                    } else {
                        assert!(
                            view.facts
                                .effective(
                                    &caller().map_err(|_| ErrorCode::AuthorityUnavailable)?,
                                    &tenant_id()?,
                                    chrono::Utc::now()
                                )
                                .is_err()
                        );
                    }
                    Ok(())
                })
                .map_err(failure)?;
        }
    }
    Ok(())
}

fn tenant_id() -> Result<TenantId> {
    "a".try_into().map_err(|_| ErrorCode::AuthorityUnavailable)
}

#[test]
fn oauth_absence_publication_and_mismatched_revision_remain_distinct() -> anyhow::Result<()> {
    let initial = header()?;
    let authority = authority(&initial)?;
    let old = super::oauth::metadata(&initial)?;
    let dependencies = std::slice::from_ref(&old.reference);
    let source = authority
        .register(prepared(&initial, dependencies)?, 0)
        .map_err(failure)?;
    authority
        .retain_prepared(&caller()?, &source, dependencies)
        .map_err(failure)?;
    let request = reused(&authority)?;
    let mut changed = old.clone();
    changed.generation = 1;
    authority
        .prepare(initial.clone(), &[], Some((&old, &changed)))
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    let latest = reused(&authority)?;
    for lease in [&source, &request.lease, &latest.lease] {
        authority
            .read()
            .map_err(failure)?
            .check(lease, |view| {
                assert!(view.covers(dependencies, DependencyIntent::Fresh)?);
                assert!(view.oauth(&old.reference)? == Some(&changed));
                Ok(())
            })
            .map_err(failure)?;
    }
    assert_eq!(
        authority.read().map_err(failure)?.header().revision,
        initial.revision
    );
    let mut conflict = changed.clone();
    conflict.reference.revision_id = "v2".try_into()?;
    authority
        .prepare(initial, &[], Some((&changed, &conflict)))
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    assert_eq!(
        authority
            .read()
            .map_err(failure)?
            .check(&request.lease, |view| view
                .covers(dependencies, DependencyIntent::Fresh)),
        Err(ErrorCode::CapabilityMismatch)
    );
    Ok(())
}

#[test]
fn optional_binding_and_new_managed_link_require_the_actual_caller_closure() -> anyhow::Result<()> {
    let initial = header()?;
    let authority = authority(&initial)?;
    let mut audit = prepared(&initial, &[])?;
    audit.facts.bindings.clear();
    for record in audit.facts.credentials.values_mut() {
        record.binding = None;
        record.audit_reader = true;
    }
    let source = authority
        .register(View::new(audit.facts, Vec::new()).map_err(failure)?, 0)
        .map_err(failure)?;
    authority
        .retain_prepared(&caller()?, &source, &[])
        .map_err(failure)?;
    drop(reused(&authority)?);
    // A publication can introduce a new binding/managed-principal link that the
    // old view never loaded. It must miss, not infer that the linked row exists.
    let mut bound = prepared(&initial, &[])?;
    let credential = bound
        .facts
        .credentials
        .pop_first()
        .ok_or_else(|| anyhow::anyhow!("credential"))?
        .1;
    authority
        .prepare(initial.clone(), &[Change::Credential(credential)], None)
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    assert!(
        authority
            .try_reuse_token(&hash())
            .map_err(failure)?
            .is_none()
    );
    authority
        .replace(&source, prepared(&initial, &[])?, 1)
        .map_err(failure)?;
    authority
        .retain_prepared(&caller()?, &source, &[])
        .map_err(failure)?;
    let mut binding = bound
        .facts
        .bindings
        .pop_first()
        .ok_or_else(|| anyhow::anyhow!("binding"))?
        .1;
    binding.managed_by = Some(model::ManagedBy {
        principal: "new-manager".try_into()?,
        class: model::AuthorityClass::Tenant,
    });
    authority
        .prepare(initial, &[Change::Binding(binding)], None)
        .map_err(failure)?
        .publish()
        .map_err(failure)?;
    assert!(
        authority
            .try_reuse_token(&hash())
            .map_err(failure)?
            .is_none()
    );
    Ok(())
}
