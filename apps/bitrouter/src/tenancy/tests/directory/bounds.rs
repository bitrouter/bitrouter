use super::*;

#[tokio::test]
async fn directory_empty_and_exact_full_pages_have_no_spurious_continuation() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = Service::open(db, None).await?;
    let installation = service.current().map_err(failure)?.installation.clone();
    let server = Server::start(&service).await?;
    let empty = server.page(&installation).await?;
    ensure!(empty.entries.is_empty() && empty.next_after.is_none());
    for index in 0..100 {
        server
            .client
            .put_principal(
                &format!("principal-{index:03}").try_into()?,
                &PrincipalPut {
                    operation_id: format!("principal-{index:03}").try_into()?,
                    expected_revision: None,
                    issuer: "synthetic".into(),
                    subject: format!("subject-{index}"),
                    installation_id: installation.clone(),
                },
            )
            .await?;
    }
    let full = server.page(&installation).await?;
    ensure!(full.entries.len() == 100 && full.next_after.is_none());
    let last = full.entries.last().context("last entry")?.cursor();
    let end = server
        .client
        .directory(
            &installation,
            Some(full.authority_revision),
            Some(&last),
            100,
        )
        .await?;
    ensure!(end.entries.is_empty() && end.next_after.is_none());
    service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn directory_byte_budget_shortens_pages_without_skipping_entries() -> Result<()> {
    let fixture = Fixture::new().await?;
    let server = Server::start(&fixture.service).await?;
    let scope = GrantScope::Tenant {
        operator_pools: (0..64)
            .map(|index| {
                Ok(ResourceKey {
                    owner_kind: OwnerKind::OperatorPool,
                    owner_id: "o".repeat(128).try_into()?,
                    resource_kind: ResourceKind::Route,
                    resource_id: format!("{index:03}{}", "r".repeat(125)).try_into()?,
                })
            })
            .collect::<Result<_>>()?,
    };
    for index in 0..64 {
        server
            .client
            .put_binding(
                &fixture.tenant,
                &format!("large-{index:02}").try_into()?,
                &BindingPut {
                    operation_id: format!("large-binding-{index}").try_into()?,
                    expected_revision: None,
                    principal_id: fixture.principal.clone(),
                    role: TenantRole::Viewer,
                    scope: scope.clone(),
                    expires_at: expiry()?,
                    expected_management_owner: None,
                    installation_id: fixture.installation.clone(),
                },
            )
            .await?;
    }
    let first = server.page(&fixture.installation).await?;
    ensure!(first.entries.len() < 67 && first.next_after.is_some());
    ensure!(serde_json::to_vec(&first)?.len() <= MAX_DIRECTORY_PAGE_BYTES);
    let second = server
        .client
        .directory(
            &fixture.installation,
            Some(first.authority_revision),
            first.next_after.as_ref(),
            100,
        )
        .await?;
    ensure!(second.next_after.is_none());
    ensure!(serde_json::to_vec(&second)?.len() <= MAX_DIRECTORY_PAGE_BYTES);
    let all: Vec<_> = first.entries.iter().chain(&second.entries).collect();
    ensure!(all.len() == 67 && all.windows(2).all(|pair| pair[0].key() < pair[1].key()));
    // The cut is caused by exact JSON bytes, including the envelope and cursor.
    let mut oversized = first.clone();
    oversized
        .entries
        .push(second.entries.first().context("remaining entry")?.clone());
    oversized.next_after = oversized.entries.last().map(DirectoryEntryV1::cursor);
    ensure!(serde_json::to_vec(&oversized)?.len() > MAX_DIRECTORY_PAGE_BYTES);
    ensure!(oversized.validate().is_err());
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn directory_cancelled_reader_retains_transaction_and_read_permit_until_cleanup() -> Result<()>
{
    use crate::tenancy::transaction::{Disposition, Phase};
    let fixture = Fixture::new().await?;
    fixture.issue("directory-cancel-live").await?;
    let service = fixture.service.clone();
    let caller = fixture.operator.clone();
    let gate = service.cleanup.hold(Phase::Active);
    let waiter = tokio::spawn(async move { service.directory(&caller, None, None, 100).await });
    tokio::time::timeout(std::time::Duration::from_secs(3), gate.entered.notified()).await?;
    ensure!(fixture.service.reads.available_permits() == 7);
    waiter.abort();
    ensure!(waiter.await.is_err_and(|error| error.is_cancelled()));
    ensure!(fixture.service.cleanup.disposition() == Disposition::Pending);
    fixture.service.cleanup.release_gate();
    let page = fixture
        .service
        .directory(&fixture.operator, None, None, 100)
        .await
        .map_err(failure)?;
    ensure!(page.entries.len() == 4);
    ensure!(fixture.service.cleanup.disposition() == Disposition::RolledBack);
    ensure!(fixture.service.reads.available_permits() == 8);
    fixture.service.shutdown().await?;
    Ok(())
}
