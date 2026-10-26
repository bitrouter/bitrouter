use super::*;
fn reference() -> anyhow::Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: bitrouter_ai::tenancy::authority::OwnerKind::Tenant,
        owner_id: "t".try_into()?,
        resource_kind: bitrouter_ai::tenancy::authority::ResourceKind::Credential,
        resource_id: "slot".try_into()?,
        revision_id: "r1".try_into()?,
    })
}
#[tokio::test]
async fn armed_drop_is_unknown_and_reserved_drop_is_not_sent_without_a_model_attempt()
-> anyhow::Result<()> {
    for armed in [false, true] {
        let journal = Journal::new();
        let mut permit = journal
            .reserve(reference()?, "a".repeat(64), 0)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        if armed {
            permit.arm([9; 32]).map_err(|e| anyhow::anyhow!("{e:?}"))?;
        }
        drop(permit);
        journal
            .close()
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        let Snapshot::Refresh(facts) = journal.snapshot().map_err(|e| anyhow::anyhow!("{e:?}"))?
        else {
            anyhow::bail!("expected refresh evidence");
        };
        assert!(
            facts[0].outcome
                == if armed {
                    Outcome::Unknown
                } else {
                    Outcome::NotSent
                }
        );
        assert!(journal.reserve(reference()?, "b".repeat(64), 1).is_err());
    }
    Ok(())
}
#[tokio::test]
async fn observed_http_rejection_survives_lost_body_and_aux_close_waits_for_actual_owner()
-> anyhow::Result<()> {
    let journal = Journal::new();
    let mut permit = journal
        .reserve(reference()?, "a".repeat(64), 0)
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    permit.arm([7; 32]).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    permit.status(403).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(1), journal.close())
            .await
            .is_err()
    );
    drop(permit);
    journal
        .close()
        .await
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let Snapshot::Refresh(facts) = journal.snapshot().map_err(|e| anyhow::anyhow!("{e:?}"))? else {
        anyhow::bail!("expected refresh evidence");
    };
    assert!(facts[0].outcome == Outcome::Rejected && facts[0].http_status == Some(403));
    Ok(())
}
