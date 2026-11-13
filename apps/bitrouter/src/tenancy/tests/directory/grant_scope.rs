use super::*;
use bitrouter_ai::tenancy::authority::MAX_GRANT_SCOPE_BYTES;

// Real SQL lifecycle rows for the 69-root configuration's full dependency set:
// 69 routes, four profiles per route and one shared credential. These tests
// target the two metadata loaders, not the separately bounded route compiler.
async fn lifecycle_configuration(
    fixture: &Fixture,
) -> Result<(
    bitrouter_ai::tenancy::configuration::ConfigurationPut,
    Vec<bitrouter_ai::tenancy::configuration::ResourceRef>,
)> {
    use crate::tenancy::resources::lifecycle::{self, Record};
    let revision = fixture.service.current().map_err(failure)?.revision;
    let mut references = Vec::new();
    for index in 0..346 {
        let kind = match index {
            0..69 => ResourceKind::Route,
            69..345 => ResourceKind::Policy,
            _ => ResourceKind::Credential,
        };
        let reference = bitrouter_ai::tenancy::configuration::ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: format!("r{index:03}{}", "x".repeat(64)).try_into()?,
            revision_id: "lifecycle-r1".try_into()?,
        };
        lifecycle::write(
            &fixture.db,
            &fixture.installation,
            &Record {
                reference: reference.clone(),
                is_head: true,
                revoked: index == 344,
                authority_revision: revision,
            },
        )
        .await?;
        references.push(reference);
    }
    let mut configuration = fixture.configuration_input("lifecycle-69-roots")?;
    configuration.document.routes = references.iter().take(69).cloned().collect();
    configuration.validate()?;
    ensure!(configuration.document.routes.len() == 69 && references.len() == 346);
    Ok((configuration, references))
}

/// SDV for 346 real lifecycle facts under the unchanged default read budget.
#[tokio::test]
async fn lifecycle_authority_load_346_keeps_duplicates_capacity_and_unknown_distinct() -> Result<()>
{
    use crate::tenancy::store::directory::{self, Lookup, Presence};
    use sea_orm::{ConnectionTrait, TransactionTrait};
    let fixture = Fixture::new().await?;
    let (_, references) = lifecycle_configuration(&fixture).await?;
    let header = fixture.service.current().map_err(failure)?.header();
    let bytes = fixture.service.authority.load_budget() / 3;
    let mut facts =
        directory::load_caller_in(&fixture.db, &header, &fixture.operator, bytes).await?;
    directory::load_dependencies_in(&fixture.db, &mut facts, &[], 0).await?;
    facts.snapshot.resource_lifecycle.restrict();
    let absent = bitrouter_ai::tenancy::configuration::ResourceRef {
        resource_id: "not-yet-observed".try_into()?,
        ..references.first().context("first reference")?.clone()
    };
    ensure!(facts.snapshot.resource_lifecycle.revoked(&absent));
    let refused = directory::load_dependencies_in(&fixture.db, &mut facts, &references, 1).await;
    ensure!(refused.is_err_and(|error| error.is::<directory::CapacityExceeded>()));
    ensure!(facts.snapshot.resource_lifecycle.len() == 0 && facts.loaded.is_empty());

    // The old Credential-sized fair-share calculation refuses this exact call.
    directory::load_dependencies_in(&fixture.db, &mut facts, &references, bytes).await?;
    ensure!(facts.snapshot.resource_lifecycle.len() == 346 && facts.loaded.len() == 346);
    for reference in &references {
        let record = facts
            .snapshot
            .resource_lifecycle
            .record(reference)
            .context("loaded record")?;
        ensure!(
            record.reference == *reference
                && record.is_head
                && record.authority_revision == header.revision
        );
        ensure!(
            facts.snapshot.resource_lifecycle.head(&reference.key())
                == Some(&reference.revision_id)
        );
        ensure!(facts.presence(&Lookup::Resource(reference.clone())) == Presence::Present);
    }
    ensure!(
        facts
            .snapshot
            .resource_lifecycle
            .revoked(references.get(344).context("revoked profile")?)
    );

    // Duplicate inputs retain one fact. With the SQL table unavailable, known
    // facts still need no read; a new reference must remain unknown on error.
    let tx = fixture.db.begin().await?;
    tx.execute_unprepared("DROP TABLE tenancy_resource_lifecycle")
        .await?;
    let duplicate = references.first().context("first reference")?.clone();
    directory::load_dependencies_in(&tx, &mut facts, &[duplicate.clone(), duplicate], bytes)
        .await?;
    let unknown =
        directory::load_dependencies_in(&tx, &mut facts, std::slice::from_ref(&absent), bytes)
            .await;
    ensure!(unknown.is_err_and(|error| !error.is::<directory::CapacityExceeded>()));
    ensure!(!facts.snapshot.resource_lifecycle.loaded(&absent));
    ensure!(facts.presence(&Lookup::Resource(absent.clone())) == Presence::NotLoaded);
    tx.rollback().await?;
    directory::load_dependencies_in(
        &fixture.db,
        &mut facts,
        std::slice::from_ref(&absent),
        bytes,
    )
    .await?;
    ensure!(facts.presence(&Lookup::Resource(absent.clone())) == Presence::Absent);
    ensure!(
        facts.snapshot.resource_lifecycle.loaded(&absent)
            && !facts.snapshot.resource_lifecycle.revoked(&absent)
    );
    ensure!(facts.snapshot.resource_lifecycle.len() == 346);
    ensure!(fixture.service.current().map_err(failure)?.revision == header.revision);
    fixture.service.shutdown().await?;
    Ok(())
}

/// SDV for configuration preparation extending its actual 346-row lifecycle closure.
#[tokio::test]
async fn lifecycle_configuration_69_roots_extends_346_without_losing_head_or_revision_fences()
-> Result<()> {
    use crate::tenancy::{model::Snapshot, store::directory};
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement, TransactionTrait};
    let fixture = Fixture::new().await?;
    let (configuration, references) = lifecycle_configuration(&fixture).await?;
    let header = fixture.service.current().map_err(failure)?.header();
    let bytes = fixture.service.authority.load_budget() / 3;
    let command = Command::Configuration(fixture.tenant.clone(), configuration);
    let mut facts =
        directory::load_command_in(&fixture.db, &header, &fixture.operator, &command, bytes)
            .await?;
    directory::extend_dependencies_in(&fixture.db, &mut facts.snapshot, &[], 0).await?;
    let refused =
        directory::extend_dependencies_in(&fixture.db, &mut facts.snapshot, &references, 1).await;
    ensure!(refused.is_err_and(|error| error.is::<directory::CapacityExceeded>()));
    ensure!(facts.snapshot.resource_lifecycle.len() == 69);

    // This is the metadata extension used by configuration preparation, with
    // the unchanged 64 MiB default's existing third. The compiler is not mocked
    // or invoked here; the Cloud full-catalog case covers that separate phase.
    directory::extend_dependencies_in(&fixture.db, &mut facts.snapshot, &references, bytes).await?;
    ensure!(facts.snapshot.resource_lifecycle.len() == 346);
    for reference in &references {
        let record = facts
            .snapshot
            .resource_lifecycle
            .record(reference)
            .context("loaded record")?;
        ensure!(
            record.reference == *reference
                && record.is_head
                && record.authority_revision == header.revision
        );
        ensure!(
            facts.snapshot.resource_lifecycle.head(&reference.key())
                == Some(&reference.revision_id)
        );
    }
    let tx = fixture.db.begin().await?;
    let reference = references.first().context("first reference")?;
    tx.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_resource_lifecycle SET authority_revision=? WHERE resource_id=?",
        vec![
            i64::try_from(header.revision.get() + 1)?.into(),
            reference.resource_id.to_string().into(),
        ],
    ))
    .await?;
    let mut unpublished = Snapshot::from_header(&header);
    let future = directory::extend_dependencies_in(
        &tx,
        &mut unpublished,
        std::slice::from_ref(reference),
        bytes,
    )
    .await;
    ensure!(future.is_err_and(|error| !error.is::<directory::CapacityExceeded>()));
    ensure!(!unpublished.resource_lifecycle.loaded(reference));
    tx.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "UPDATE tenancy_resource_lifecycle SET authority_revision=?,installation_id=? WHERE resource_id=?",
        vec![i64::try_from(header.revision.get())?.into(), "foreign-installation".into(), reference.resource_id.to_string().into()],
    )).await?;
    let foreign = directory::extend_dependencies_in(
        &tx,
        &mut unpublished,
        std::slice::from_ref(reference),
        bytes,
    )
    .await;
    ensure!(foreign.is_err_and(|error| !error.is::<directory::CapacityExceeded>()));
    ensure!(!unpublished.resource_lifecycle.loaded(reference));
    tx.execute_unprepared("DROP TABLE tenancy_resource_lifecycle")
        .await?;
    directory::extend_dependencies_in(
        &tx,
        &mut facts.snapshot,
        &[reference.clone(), reference.clone()],
        bytes,
    )
    .await?;
    let missing = bitrouter_ai::tenancy::configuration::ResourceRef {
        resource_id: "unknown-after-drop".try_into()?,
        ..reference.clone()
    };
    let unknown = directory::extend_dependencies_in(
        &tx,
        &mut facts.snapshot,
        std::slice::from_ref(&missing),
        bytes,
    )
    .await;
    ensure!(unknown.is_err_and(|error| !error.is::<directory::CapacityExceeded>()));
    ensure!(!facts.snapshot.resource_lifecycle.loaded(&missing));
    tx.rollback().await?;
    ensure!(facts.snapshot.resource_lifecycle.len() == 346);
    ensure!(fixture.service.current().map_err(failure)?.revision == header.revision);
    fixture.service.shutdown().await?;
    Ok(())
}

fn large_scope() -> Result<GrantScope> {
    let scope = GrantScope::Resources {
        resources: (0..748)
            .map(|index| {
                Ok(ResourceKey {
                    owner_kind: OwnerKind::OperatorPool,
                    owner_id: "o".repeat(128).try_into()?,
                    resource_kind: ResourceKind::Configuration,
                    resource_id: format!("{index:03}{}", "r".repeat(125)).try_into()?,
                })
            })
            .collect::<Result<_>>()?,
    };
    let bytes = serde_json::to_vec(&scope)?.len();
    ensure!((MAX_GRANT_SCOPE_BYTES - 1024..=MAX_GRANT_SCOPE_BYTES).contains(&bytes));
    scope.validate()?;
    Ok(scope)
}

fn binding(fixture: &Fixture, operation: &str, scope: GrantScope) -> Result<BindingPut> {
    Ok(BindingPut {
        operation_id: operation.try_into()?,
        expected_revision: None,
        principal_id: fixture.principal.clone(),
        role: TenantRole::TenantAdmin,
        scope,
        expires_at: expiry()?,
        expected_management_owner: None,
        installation_id: fixture.installation.clone(),
    })
}

/// SDV for default-budget management, once-only issuance, exact live authorization and restart.
#[tokio::test]
async fn large_scope_default_budget_issues_once_authorizes_exactly_and_reopens() -> Result<()> {
    let fixture = Fixture::new().await?;
    let server = Server::start(&fixture.service).await?;
    let scope = large_scope()?;
    let binding_id: BindingId = "wide-binding".try_into()?;
    let input = binding(&fixture, "wide-binding-put", scope.clone())?;
    let created = server
        .client
        .put_binding(&fixture.tenant, &binding_id, &input)
        .await?;
    let repeated = server
        .client
        .put_binding(&fixture.tenant, &binding_id, &input)
        .await?;
    ensure!(created.authority_revision == repeated.authority_revision);
    ensure!(created.resource_revision == repeated.resource_revision);
    let mut foreign_scope = scope.clone();
    if let GrantScope::Resources { resources } = &mut foreign_scope {
        let key = resources.last_mut().context("last resource")?;
        key.owner_kind = OwnerKind::Tenant;
        key.owner_id = "other-tenant".try_into()?;
    }
    let foreign = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Binding(
                fixture.tenant.clone(),
                "foreign-binding".try_into()?,
                binding(&fixture, "foreign-wide-put", foreign_scope)?,
            ),
        )
        .await
        .map_err(failure)?;
    ensure!(
        matches!(foreign, StoredResponse::Failure(error) if error.code == ErrorCode::Forbidden)
    );
    ensure!(
        fixture.service.current().map_err(failure)?.revision
            == created.authority_revision.context("binding revision")?
    );
    let issue = CredentialCreate {
        operation_id: "wide-issue".try_into()?,
        expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
        principal_id: fixture.principal.clone(),
        permissions: vec![Permission::ResourceRead],
        scope: scope.clone(),
        expires_at: input.expires_at,
        rotate_credential_id: None,
        binding_id,
        installation_id: fixture.installation.clone(),
    };
    let (first, second) = tokio::join!(
        server.client.issue_credential(&fixture.tenant, &issue),
        server.client.issue_credential(&fixture.tenant, &issue),
    );
    let first = first?;
    let second = second?;
    ensure!(first.credential_id == second.credential_id);
    ensure!(usize::from(first.secret.is_some()) + usize::from(second.secret.is_some()) == 1);
    let token = first.secret.or(second.secret).context("initial secret")?;
    let before_denial = fixture.service.current().map_err(failure)?.revision;
    let mut outside = issue.clone();
    outside.operation_id = "wide-overgrant".try_into()?;
    outside.expected_authority_revision = before_denial;
    if let GrantScope::Resources { resources } = &mut outside.scope {
        resources.last_mut().context("last resource")?.resource_id =
            "outside-binding".try_into()?;
    }
    let denied = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Issue(fixture.tenant.clone(), outside),
        )
        .await
        .map_err(failure)?;
    ensure!(matches!(denied, StoredResponse::Failure(error) if error.code == ErrorCode::Forbidden));
    ensure!(fixture.service.current().map_err(failure)?.revision == before_denial);
    let authenticated = fixture
        .service
        .authenticate_live(&token)
        .await
        .map_err(failure)?;
    let header = fixture.service.current().map_err(failure)?.header();
    let refused = crate::tenancy::store::directory::load_caller_in(
        &fixture.db,
        &header,
        &authenticated.caller,
        64 * 1024,
    )
    .await;
    ensure!(
        refused
            .is_err_and(|error| error.is::<crate::tenancy::store::directory::CapacityExceeded>())
    );
    ensure!(fixture.service.current().map_err(failure)?.revision == header.revision);
    let GrantScope::Resources { resources } = &scope else {
        anyhow::bail!("explicit resource scope missing");
    };
    // Exercise a key after the former 256-entry boundary, with all identity
    // components checked independently of cardinality and the directory read.
    let allowed = resources.last().context("last granted key")?.clone();
    let authorize = |key: ResourceKey, permission| {
        fixture.service.authority.read().and_then(|mut guard| {
            guard.check(&authenticated.lease, |view| {
                view.facts().authorize(
                    &authenticated.caller,
                    &fixture.tenant,
                    permission,
                    &GrantScope::Resources {
                        resources: vec![key],
                    },
                    Utc::now(),
                )
            })
        })
    };
    ensure!(authorize(allowed.clone(), Permission::ResourceRead).is_ok());
    ensure!(authorize(allowed.clone(), Permission::ExecutionSubmit) == Err(ErrorCode::Forbidden));
    let mut foreign = allowed.clone();
    foreign.owner_id = "other-pool".try_into()?;
    ensure!(authorize(foreign, Permission::ResourceRead) == Err(ErrorCode::Forbidden));
    let mut wrong_kind = allowed.clone();
    wrong_kind.resource_kind = ResourceKind::Route;
    ensure!(authorize(wrong_kind, Permission::ResourceRead) == Err(ErrorCode::Forbidden));
    let mut wrong_id = allowed.clone();
    wrong_id.resource_id = "not-granted".try_into()?;
    ensure!(authorize(wrong_id, Permission::ResourceRead) == Err(ErrorCode::Forbidden));
    let mut wrong_owner = allowed;
    wrong_owner.owner_kind = OwnerKind::Tenant;
    ensure!(authorize(wrong_owner, Permission::ResourceRead) == Err(ErrorCode::Forbidden));

    // Include the tenant/active dispatch read set, not only token authentication.
    let facts = crate::tenancy::store::directory::load_tenant_in(
        &fixture.db,
        &header,
        &authenticated.caller,
        &fixture.tenant,
        true,
        fixture.service.authority.load_budget() / 3,
    )
    .await?;
    ensure!(
        facts
            .snapshot
            .credentials
            .get(&first.credential_id)
            .context("loaded credential")?
            .scope
            == scope
    );
    drop(server);
    fixture.service.shutdown().await?;
    let restarted = Service::open(fixture.db.clone(), None).await?;
    restarted.authenticate_live(&token).await.map_err(failure)?;
    let server = Server::start(&restarted).await?;
    let replay = server
        .client
        .issue_credential(&fixture.tenant, &issue)
        .await?;
    ensure!(replay.credential_id == first.credential_id && replay.secret.is_none());
    let page = server.page(&fixture.installation).await?;
    ensure!(page.entries.iter().any(|entry| matches!(
        entry, DirectoryEntryV1::Credential { credential_id, scope: observed, .. }
        if credential_id == &first.credential_id && observed == &scope
    )));
    restarted.shutdown().await?;
    Ok(())
}

/// SDV for read-only capacity retries and exact sentinel/cursor continuation.
#[tokio::test]
async fn large_scope_directory_shortens_reads_and_resumes_without_skipping() -> Result<()> {
    let fixture = Fixture::new().await?;
    let server = Server::start(&fixture.service).await?;
    for index in 0..6 {
        server
            .client
            .put_binding(
                &fixture.tenant,
                &format!("wide-{index}").try_into()?,
                &binding(&fixture, &format!("wide-put-{index}"), large_scope()?)?,
            )
            .await?;
    }
    let revision = fixture.service.current().map_err(failure)?.revision;
    let mut after = None;
    let mut observed = Vec::new();
    for _ in 0..10 {
        let page = server
            .client
            .directory(&fixture.installation, Some(revision), after.as_ref(), 100)
            .await?;
        ensure!(page.authority_revision == revision);
        ensure!(serde_json::to_vec(&page)?.len() <= MAX_DIRECTORY_PAGE_BYTES);
        ensure!(page.entries.len() <= 100);
        ensure!(
            after
                .as_ref()
                .is_none_or(|cursor| page.entries.iter().all(|entry| entry.key() > cursor.key()))
        );
        let next = page.next_after.clone();
        ensure!(next.as_ref().is_none_or(|cursor| {
            page.entries
                .last()
                .is_some_and(|entry| entry.cursor() == *cursor)
        }));
        observed.extend(page.entries);
        after = next;
        if after.is_none() {
            break;
        }
    }
    ensure!(after.is_none());
    ensure!(
        observed.len() == 9
            && observed
                .windows(2)
                .all(|pair| pair[0].key() < pair[1].key())
    );
    ensure!(
        observed
            .iter()
            .filter(|entry| matches!(entry, DirectoryEntryV1::Binding { .. }))
            .count()
            == 7
    );
    ensure!(fixture.service.current().map_err(failure)?.revision == revision);
    fixture.service.shutdown().await?;
    Ok(())
}

/// SDV for exhausted one-row capacity refusing without fencing healthy authority.
#[tokio::test]
async fn directory_one_row_capacity_refuses_without_skipping_or_storage_fence() -> Result<()> {
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
    let fixture = Fixture::new().await?;
    // Deliberately oversized storage: the SQL projection must refuse before
    // transferring or decoding it, even after the effective limit reaches one.
    fixture
        .db
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tenancy_bindings SET scope_json=? WHERE tenant_id=? AND id=?",
            [
                "x".repeat(4 * 1024 * 1024).into(),
                fixture.tenant.to_string().into(),
                fixture.binding.to_string().into(),
            ],
        ))
        .await?;
    let read = fixture
        .service
        .directory(&fixture.operator, None, None, 100)
        .await;
    ensure!(matches!(read, Err(error) if error.0.code == ErrorCode::CapacityExceeded));
    let revision = fixture.service.current().map_err(failure)?.revision;
    let after = format!("v1|binding|{}|{}", fixture.tenant, fixture.binding).try_into()?;
    let page = fixture
        .service
        .directory(&fixture.operator, Some(revision), Some(after), 100)
        .await
        .map_err(failure)?;
    ensure!(page.entries.len() == 2 && page.next_after.is_none());
    fixture.service.shutdown().await?;
    Ok(())
}
