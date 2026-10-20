//! Cleanup authority never grants a new forward publication lease.

use super::*;
use crate::gateway::shared::continuation::ProducerClosure;

/// The caller retains receipt-matched cancellation evidence before awaiting
/// cleanup. Joining producers alone authorizes only prepublication cleanup.
pub(in crate::continuation) enum Authority<'a> {
    Cancelled(&'a NativeIntent),
    /// A confirmed bind lost forward authority without positive delivery
    /// failure. Only expired prepublication or existing cleanup is reclaimable.
    Abandoned(&'a NativeIntent),
    Closed {
        intent: &'a NativeIntent,
        closure: &'a ProducerClosure,
    },
}

impl Authority<'_> {
    fn intent(&self) -> &NativeIntent {
        match self {
            Self::Cancelled(intent) | Self::Abandoned(intent) | Self::Closed { intent, .. } => {
                intent
            }
        }
    }
}

/// Missing/foreign observations do not by themselves resolve an uncertain bind.
/// Reserved intents require the confirmed producer-close barrier first.
pub(in crate::continuation) enum Observation {
    Missing,
    Foreign,
    Retained,
    Expired,
    Claimed(Box<Claim>),
}

pub(in crate::continuation) struct Claim {
    namespace: String,
    snapshot: Snapshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::continuation) enum Finished {
    Removed,
    Missing,
    Changed,
}

#[derive(Default)]
pub(in crate::continuation) struct Recovery {
    pub scanned: usize,
    pub removed: u64,
    pub failed: u64,
}

impl SharedNative {
    /// A matching lease is not required for cleanup; immutable intent identity
    /// and the caller's cancellation/closed-producer evidence are required.
    pub(in crate::continuation) async fn claim_cleanup(
        &self,
        authority: Authority<'_>,
    ) -> Result<Observation> {
        ensure!(
            authority.intent().producer == self.authority.producer_id(),
            "cleanup intent belongs to another producer"
        );
        self.claim_cleanup_inner(&authority.intent().identity, Some(&authority), None)
            .await
    }

    async fn claim_cleanup_inner(
        &self,
        identity: &str,
        authority: Option<&Authority<'_>>,
        hint: Option<&Snapshot>,
    ) -> Result<Observation> {
        self.authority
            .retry(|| async {
                let tx = match authority {
                    Some(Authority::Closed { closure, .. }) => {
                        self.authority
                            .begin_closed_identity(closure, identity)
                            .await?
                    }
                    _ => self.authority.begin_cleanup(identity).await?,
                };
                let Some(snapshot) = self.lock_row(&tx, identity).await? else {
                    tx.commit().await?;
                    return Ok(Observation::Missing);
                };
                let plaintext = self.decrypt(&snapshot.row)?;
                if authority.is_some_and(|proof| !proof.intent().owns(&snapshot.row)) {
                    tx.commit().await?;
                    return Ok(Observation::Foreign);
                }
                if hint.is_some_and(|hint| {
                    hint.body != snapshot.body || hint.revision != snapshot.revision
                }) {
                    tx.commit().await?;
                    return Ok(Observation::Retained);
                }
                if snapshot.row.publication_state == PUBLICATION_EXPIRED {
                    tx.commit().await?;
                    return Ok(Observation::Expired);
                }
                let now = SharedContinuationDb::now(&tx).await?;
                if parse_timestamp(&snapshot.row.expires_at)? <= now {
                    ensure!(self.scrub(&tx, &snapshot).await?, Rejection::Changed);
                    tx.commit().await?;
                    return Ok(Observation::Expired);
                }
                let snapshot = if snapshot.row.publication_state == PUBLICATION_CLEANUP {
                    snapshot
                } else {
                    let eligible = match snapshot.row.publication_state.as_str() {
                        PUBLICATION_ACTIVE => matches!(authority, Some(Authority::Cancelled(_))),
                        PUBLICATION_PROVISIONAL | PUBLICATION_DELIVERING => {
                            matches!(
                                authority,
                                Some(Authority::Cancelled(_) | Authority::Closed { .. })
                            ) || parse_timestamp(&snapshot.row.publication_lease_until)? <= now
                        }
                        _ => false,
                    };
                    if !eligible {
                        tx.commit().await?;
                        return Ok(Observation::Retained);
                    }
                    self.save_cleanup(
                        &tx,
                        &snapshot,
                        &plaintext,
                        !matches!(
                            authority,
                            Some(Authority::Cancelled(_) | Authority::Closed { .. })
                        ),
                    )
                    .await?
                };
                tx.commit().await?;
                Ok(Observation::Claimed(Box::new(Claim {
                    namespace: self.authority.namespace().into(),
                    snapshot,
                })))
            })
            .await
    }

    async fn save_cleanup(
        &self,
        tx: &DatabaseTransaction,
        previous: &Snapshot,
        plaintext: &[u8],
        require_expired_lease: bool,
    ) -> Result<Snapshot> {
        let mut row = previous.row.clone();
        row.publication_state = PUBLICATION_CLEANUP.into();
        self.seal(&mut row, plaintext)?;
        let body = serde_json::to_string(&row)?;
        ensure!(
            body.len() <= MAX_NATIVE_BYTES,
            "shared native record exceeds byte limit"
        );
        let revision = previous
            .revision
            .checked_add(1)
            .context("shared native revision exhausted")?;
        // Demotion and cleanup acquisition are one durable write. A crash can
        // never strand a newly claimed active row without its cancellation state.
        // https://www.postgresql.org/docs/current/queries-with.html#QUERIES-WITH-CTE-MATERIALIZATION
        let changed = tx
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "WITH decision_clock AS MATERIALIZED (SELECT clock_timestamp() AS t)
             UPDATE gateway_shared_native_continuations AS c
             SET revision = $1, body = $2, publication_state = $3
             FROM decision_clock AS n
             WHERE c.namespace = $4 AND c.identity = $5 AND c.revision = $6 AND c.body = $7
               AND (NOT $8::boolean OR c.lease_until::timestamptz <= n.t)
               AND c.expires_at::timestamptz > n.t",
                [
                    revision.into(),
                    body.clone().into(),
                    PUBLICATION_CLEANUP.into(),
                    self.authority.namespace().into(),
                    row.continuation_identity.clone().into(),
                    previous.revision.into(),
                    previous.body.clone().into(),
                    require_expired_lease.into(),
                ],
            ))
            .await?;
        ensure!(changed.rows_affected() == 1, Rejection::Changed);
        Ok(Snapshot {
            body,
            revision,
            row,
        })
    }

    /// Any authorized recovery worker can finish a durable cleanup claim.
    /// Missing/changed results are observations, not proof about a late bind.
    pub(in crate::continuation) async fn finish_cleanup(&self, claim: &Claim) -> Result<Finished> {
        ensure!(
            claim.namespace == self.authority.namespace(),
            "foreign cleanup namespace"
        );
        self.authority
            .retry(|| async {
                let tx = self
                    .authority
                    .begin_cleanup(&claim.snapshot.row.continuation_identity)
                    .await?;
                let Some(current) = self
                    .lock_row(&tx, &claim.snapshot.row.continuation_identity)
                    .await?
                else {
                    tx.commit().await?;
                    return Ok(Finished::Missing);
                };
                self.decrypt(&current.row)?;
                if current.row.publication_state != PUBLICATION_CLEANUP
                    || current.revision != claim.snapshot.revision
                    || current.body != claim.snapshot.body
                {
                    tx.commit().await?;
                    return Ok(Finished::Changed);
                }
                // Cleanup is cancellation recovery, independent of retention purge.
                // https://www.postgresql.org/docs/current/sql-delete.html
                let deleted = tx
                    .execute(Statement::from_sql_and_values(
                        DatabaseBackend::Postgres,
                        "DELETE FROM gateway_shared_native_continuations
                 WHERE namespace = $1 AND identity = $2 AND revision = $3 AND body = $4
                   AND publication_state = $5",
                        [
                            self.authority.namespace().into(),
                            current.row.continuation_identity.into(),
                            current.revision.into(),
                            current.body.into(),
                            PUBLICATION_CLEANUP.into(),
                        ],
                    ))
                    .await?;
                ensure!(deleted.rows_affected() == 1, Rejection::Changed);
                tx.commit().await?;
                Ok(Finished::Removed)
            })
            .await
    }

    /// Recover stale prepublication rows and already-demoted cleanup rows.
    /// Active rows are never selected solely because their lease expired.
    pub(in crate::continuation) async fn recover_stale(&self) -> Result<Recovery> {
        self.authority
            .bounded(async {
                let rows = self.scan_recovery().await?;
                let mut result = Recovery {
                    scanned: rows.len(),
                    ..Default::default()
                };
                for record in rows {
                    let outcome = async {
                        let identity: String = record.try_get("", "identity")?;
                        *self
                            .recovery_cursor
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Some(identity.clone());
                        let hint = self.decode_row(record, &identity)?;
                        if let Observation::Claimed(claim) = self
                            .claim_cleanup_inner(&identity, None, Some(&hint))
                            .await?
                        {
                            return self.finish_cleanup(&claim).await;
                        }
                        Ok(Finished::Changed)
                    }
                    .await;
                    match outcome {
                        Ok(Finished::Removed) => result.removed += 1,
                        Ok(Finished::Missing | Finished::Changed) => {}
                        Err(error) => {
                            result.failed += 1;
                            tracing::warn!(error = %error, "shared native recovery row failed");
                        }
                    }
                }
                if result.scanned < RECONCILIATION_BATCH_SIZE {
                    *self
                        .recovery_cursor
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = None;
                }
                Ok(result)
            })
            .await
    }

    async fn scan_recovery(&self) -> Result<Vec<sea_orm::QueryResult>> {
        let cursor = self
            .recovery_cursor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        self.authority.retry(|| async {
            let tx = self.authority.begin_read().await?;
            let now = timestamp(SharedContinuationDb::now(&tx).await?);
            // Scans supply hints only; claim_cleanup_inner revalidates each row.
            // https://www.postgresql.org/docs/current/sql-select.html
            let rows = tx.query_all(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT identity, revision, body, publication_state, lease_until, expires_at, purge_after
                 FROM gateway_shared_native_continuations
                 WHERE namespace = $1
                   AND (publication_state = $2 OR (publication_state IN ($3, $4) AND lease_until <= $5))
                   AND ($6::text IS NULL OR identity > $6)
                 ORDER BY identity LIMIT $7",
                [self.authority.namespace().into(), PUBLICATION_CLEANUP.into(),
                 PUBLICATION_PROVISIONAL.into(), PUBLICATION_DELIVERING.into(), now.into(),
                 cursor.clone().into(), i64::try_from(RECONCILIATION_BATCH_SIZE)?.into()],
            )).await?;
            tx.commit().await?;
            Ok(rows)
        }).await
    }
}

#[cfg(test)]
#[path = "tests_shared_cleanup.rs"]
mod tests;
