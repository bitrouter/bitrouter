use super::*;
use anyhow::{Context, Result, ensure};
fn code(error: ErrorCode) -> anyhow::Error {
    anyhow::anyhow!("{error:?}")
}
fn identity() -> Result<Identity> {
    Ok(Identity {
        credential: ResourceRef {
            owner_kind: bitrouter_ai::tenancy::authority::OwnerKind::Tenant,
            owner_id: "tenant".try_into()?,
            resource_kind: bitrouter_ai::tenancy::authority::ResourceKind::Credential,
            resource_id: "slot".try_into()?,
            revision_id: "r1".try_into()?,
        },
        candidate: "01".repeat(32),
    })
}
#[tokio::test]
async fn serial_count_capacity_and_rebuild_require_known_completion_and_exact_binding() -> Result<()>
{
    let journal = Journal::new();
    let identity = identity()?;
    journal.begin_counts(identity.clone()).map_err(code)?;
    let mut first = journal.claim_count([1; 32]).map_err(code)?;
    ensure!(journal.claim_count([1; 32]).is_err());
    ensure!(journal.allow_count_rebuild([2; 32]).is_err());
    first.arm("03".repeat(32), [3; 32]).map_err(code)?;
    first.status(200).map_err(code)?;
    first.counted(9).map_err(code)?;
    ensure!(journal.claim_count([2; 32]).is_err());
    journal
        .verify_count(&identity, &"03".repeat(32))
        .map_err(code)?;
    ensure!(journal.verify_count(&identity, &"04".repeat(32)).is_err());
    let mut foreign = identity.clone();
    foreign.credential.owner_id = "other-tenant".try_into()?;
    ensure!(journal.verify_count(&foreign, &"03".repeat(32)).is_err());
    ensure!(journal.allow_count_rebuild([1; 32]).is_err());
    journal.allow_count_rebuild([2; 32]).map_err(code)?;
    ensure!(journal.verify_count(&identity, &"03".repeat(32)).is_err());
    let mut second = journal.claim_count([2; 32]).map_err(code)?;
    second.arm("04".repeat(32), [4; 32]).map_err(code)?;
    second.status(200).map_err(code)?;
    second.counted(7).map_err(code)?;
    ensure!(journal.claim_count([3; 32]).is_err() && journal.allow_count_rebuild([3; 32]).is_err());
    journal
        .verify_count(&identity, &"04".repeat(32))
        .map_err(code)?;
    journal.close().await.map_err(code)?;
    ensure!(journal.verify_count(&identity, &"04".repeat(32)).is_err());
    let super::super::Snapshot::Counts(counts) = journal.snapshot().map_err(code)? else {
        anyhow::bail!("count snapshot");
    };
    ensure!(
        counts.validate()
            && counts.counts.len() == 2
            && counts.counts[0].work != counts.counts[1].work
    );
    Ok(())
}
#[tokio::test]
async fn dropped_reserved_unknown_and_rejected_producers_settle_once_and_close_wakes() -> Result<()>
{
    for status in [None, Some(200), Some(403)] {
        for armed in [false, true] {
            let journal = Journal::new();
            journal.begin_counts(identity()?).map_err(code)?;
            let mut permit = journal.claim_count([1; 32]).map_err(code)?;
            if armed {
                permit.arm("02".repeat(32), [2; 32]).map_err(code)?;
                if let Some(status) = status {
                    permit.status(status).map_err(code)?;
                }
            }
            let mut closing = Box::pin(journal.close());
            ensure!(futures::poll!(&mut closing).is_pending());
            ensure!(journal.claim_count([2; 32]).is_err());
            drop(permit);
            tokio::time::timeout(std::time::Duration::from_secs(1), closing)
                .await?
                .map_err(code)?;
            let super::super::Snapshot::Counts(counts) = journal.snapshot().map_err(code)? else {
                anyhow::bail!("count snapshot");
            };
            ensure!(counts.validate());
            let fact = counts.counts.first().context("settled fact")?;
            let expected = if !armed {
                Outcome::NotSent
            } else if status == Some(403) {
                Outcome::Rejected
            } else {
                Outcome::Unknown
            };
            ensure!(fact.outcome == expected && counts.counts.len() == 1);
            ensure!(journal.allow_count_rebuild([2; 32]).is_err());
        }
    }
    Ok(())
}
#[tokio::test]
async fn refresh_and_count_modes_are_exclusive_and_unused_second_capacity_is_clean() -> Result<()> {
    let journal = Journal::new();
    let identity = identity()?;
    journal.begin_counts(identity.clone()).map_err(code)?;
    ensure!(
        journal
            .reserve(identity.credential.clone(), "a".repeat(64), 0)
            .is_err()
    );
    let mut permit = journal.claim_count([1; 32]).map_err(code)?;
    permit.arm("02".repeat(32), [2; 32]).map_err(code)?;
    permit.status(200).map_err(code)?;
    permit.counted(0).map_err(code)?;
    journal.close().await.map_err(code)?;
    let snapshot = journal.snapshot().map_err(code)?;
    ensure!(!snapshot.unclean() && !snapshot.unknown());
    let refresh = Journal::new();
    let reserved = refresh
        .reserve(identity.credential.clone(), "a".repeat(64), 0)
        .map_err(code)?;
    ensure!(refresh.begin_counts(identity).is_err());
    drop(reserved);
    refresh.close().await.map_err(code)?;
    Ok(())
}
