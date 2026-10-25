use super::*;
use crate::{
    storage::scope::Scope,
    trajectory::store::{
        owned::OwnedStore,
        tests::{correlate_input, guarded_route_input, keyed_digest},
    },
};
use sea_orm::TransactionTrait;

#[tokio::test]
async fn outstanding_starts_are_reserved_and_may_settle_in_reverse_order() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    for observe in [true, false] {
        let (scope, provenance) = Scope::for_test(
            "installation".try_into()?,
            if observe { "observed" } else { "off" }.try_into()?,
            "creator".try_into()?,
        );
        let store = OwnedStore {
            db: db.clone(),
            scope,
            provenance,
        };
        let reservation = Reservation::new(16 * 1024, observe)?;
        let mut inputs = Vec::new();
        let mut prior = None;
        let target = 8;
        for index in 0..target {
            let mut input = correlate_input(
                &format!("request-{index}"),
                "episode",
                &format!("start-{index}"),
                guarded_route_input(&format!("route-{index}"), &format!("guard-{index}")),
            );
            input.full_input_digest = keyed_digest("key-1", &index.to_string());
            input.ancestor_prefix_digests = prior.iter().cloned().collect();
            input.starts_with_prior_turns = index > 0;
            let tx = db.begin().await?;
            let result = store
                .correlate_and_begin_reserved_in(&tx, &input, reservation, &mut io::Budget::new())
                .await;
            if let Err(error) = result {
                ensure!(observe, "Off inherited an unused Eval limit: {error:#}");
                ensure!(
                    matches!(error.downcast_ref::<Error>(), Some(Error::Capacity)),
                    "{error:#}"
                );
                // Rejection precedes all writes and leaves the transaction usable.
                ensure!(
                    store
                        .request_in(&tx, &input.request_id, &mut io::Budget::new())
                        .await?
                        .is_none()
                );
                tx.commit().await?;
                break;
            }
            tx.commit().await?;
            prior = Some(input.full_input_digest.clone());
            inputs.push(input);
        }
        ensure!(if observe {
            !inputs.is_empty() && inputs.len() < 8
        } else {
            inputs.len() == 8
        });
        for input in inputs.iter().rev() {
            let tx = db.begin().await?;
            let owner = store.owner()?;
            let settlement = store
                .settle_current_in(
                    &tx,
                    &input.request_id,
                    |request, events, sequence| {
                        let evidence = TrajectoryEvidence {
                            structural: BTreeMap::from([("settlement.duration_ms".into(), 10)]),
                            categorical: BTreeMap::new(),
                            digests: BTreeMap::new(),
                            route_measurement: None,
                            routing_evidence: Vec::new(),
                        };
                        if observe {
                            crate::trajectory::settlement::build_from_evidence(
                                &owner,
                                request,
                                events,
                                sequence,
                                evidence,
                                "2026-08-01T00:00:10Z".into(),
                                RequestStatus::Settled,
                            )
                        } else {
                            crate::trajectory::settlement::build_terminal_from_evidence(
                                &owner,
                                request,
                                sequence,
                                evidence,
                                "2026-08-01T00:00:10Z".into(),
                                RequestStatus::Settled,
                            )
                        }
                    },
                    &mut io::Budget::new(),
                )
                .await?;
            reservation.validate(&settlement)?;
            tx.commit().await?;
        }
        let tx = db.begin().await?;
        for input in &inputs {
            ensure!(
                store
                    .request_in(&tx, &input.request_id, &mut io::Budget::new())
                    .await?
                    .context("request")?
                    .status
                    == RequestStatus::Settled
            );
        }
        tx.rollback().await?;
    }
    Ok(())
}
