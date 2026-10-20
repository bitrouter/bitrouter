//! Forward publication and retention maintenance for shared native mappings.

use super::*;

impl SharedNative {
    /// Each handshake boundary independently validates live forward ownership.
    /// The caller performs delivery waits only after this transaction returns.
    pub(in crate::continuation) async fn transition(
        &self,
        publication: &PendingPublication,
        transition: ForwardTransition,
    ) -> Result<continuation_entity::Model> {
        ensure!(
            publication.phase == PendingPublicationPhase::Prepared
                && publication.instance_id == self.authority.producer_id(),
            Rejection::Changed
        );
        let (expected, next) = match transition {
            ForwardTransition::Delivering => (PUBLICATION_PROVISIONAL, PUBLICATION_DELIVERING),
            ForwardTransition::Active => (PUBLICATION_DELIVERING, PUBLICATION_ACTIVE),
        };
        self.authority
            .retry(|| async {
                let tx = self
                    .authority
                    .begin_bind(&publication.continuation_identity)
                    .await?;
                let snapshot = self
                    .lock_row(&tx, &publication.continuation_identity)
                    .await?
                    .ok_or(Rejection::Changed)?;
                let plaintext = self.decrypt(&snapshot.row)?;
                ensure!(
                    publication_owns_row(publication, &snapshot.row),
                    Rejection::Changed
                );
                ensure!(
                    snapshot.row.publication_state == expected
                        || snapshot.row.publication_state == next,
                    Rejection::Changed
                );
                let now = SharedContinuationDb::now(&tx).await?;
                ensure!(
                    parse_timestamp(&snapshot.row.publication_lease_until)? > now
                        && parse_timestamp(&snapshot.row.expires_at)? > now,
                    Rejection::Expired
                );
                let mut row = snapshot.row.clone();
                if row.publication_state != next {
                    row.publication_state = next.into();
                    self.seal(&mut row, &plaintext)?;
                    self.save_live(&tx, &snapshot, &row).await?;
                }
                tx.commit().await?;
                Ok(row)
            })
            .await
    }

    /// Retention is authorized at this locked snapshot. Publication lease age
    /// does not revoke an already active mapping. No lock follows the result.
    pub(in crate::continuation) async fn resolve(
        &self,
        owner: &str,
        request: &str,
    ) -> Result<ContinuationResolution> {
        let identity = self.identity(owner, request)?;
        let owner_identity = self.owner_identity(owner)?;
        self.authority
            .retry(|| async {
                let tx = self.authority.begin_cleanup(&identity).await?;
                let Some(snapshot) = self.lock_row(&tx, &identity).await? else {
                    tx.commit().await?;
                    return Ok(ContinuationResolution::Missing);
                };
                let plaintext = self.decrypt(&snapshot.row)?;
                ensure!(
                    snapshot.row.owner_identity == owner_identity,
                    "shared continuation owner mismatch"
                );
                let now = SharedContinuationDb::now(&tx).await?;
                if snapshot.row.publication_state == PUBLICATION_EXPIRED {
                    tx.commit().await?;
                    return Ok(ContinuationResolution::Expired);
                }
                if now >= parse_timestamp(&snapshot.row.expires_at)? {
                    ensure!(self.scrub(&tx, &snapshot).await?, Rejection::Changed);
                    tx.commit().await?;
                    return Ok(ContinuationResolution::Expired);
                }
                if snapshot.row.publication_state != PUBLICATION_ACTIVE {
                    tx.commit().await?;
                    return Ok(ContinuationResolution::Missing);
                }
                let payload = decode_payload(plaintext)?;
                let resolved = ResolvedContinuation {
                    provider_response_id: payload.provider_response_id,
                    effective_model: payload.effective_model,
                    effective_effort: payload.effective_effort,
                    effort_authoritative: payload.version >= 4,
                    causal_prefix_commitment: payload
                        .causal_prefix_commitment
                        .as_deref()
                        .and_then(CausalPrefixCommitment::parse),
                    target_fingerprint: snapshot.row.target_fingerprint.into_boxed_str(),
                    key: self.key.clone(),
                    target_namespace: Some(self.authority.namespace().into()),
                };
                tx.commit().await?;
                Ok(ContinuationResolution::Active(resolved))
            })
            .await
    }

    /// One finite pass, sharing a total deadline across scan and mutations.
    /// Cursor advancement allows later identities to progress past a bad row.
    pub(in crate::continuation) async fn maintain(&self, requested: usize) -> Result<Maintenance> {
        ensure!(
            (1..=10_000).contains(&requested),
            "invalid native maintenance batch size"
        );
        let limit = requested.min(RECONCILIATION_BATCH_SIZE);
        self.authority
            .bounded(async {
                let rows = self.scan_due(limit).await?;
                let mut result = Maintenance {
                    scanned: rows.len(),
                    ..Default::default()
                };
                for record in rows {
                    let outcome = async {
                        let identity: String = record.try_get("", "identity")?;
                        // Persist only attempted progress, before waiting on
                        // this row. Cancellation must preserve the unattempted
                        // suffix without retrying a contended first row forever.
                        *self
                            .maintenance_cursor
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = Some(identity.clone());
                        let hint = self.decode_row(record, &identity)?;
                        self.maintain_row(&hint).await
                    }
                    .await;
                    match outcome {
                        Ok(RowMaintenance::Unchanged) => {}
                        Ok(RowMaintenance::Scrubbed) => result.scrubbed += 1,
                        Ok(RowMaintenance::Purged) => result.purged += 1,
                        Err(error) => {
                            result.failed += 1;
                            tracing::warn!(error = %error, "shared native maintenance row failed");
                        }
                    }
                }
                // Wrap only after exhausting the suffix. An interrupted pass
                // retains the identity it reached instead of skipping its tail.
                if result.scanned < limit {
                    *self
                        .maintenance_cursor
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) = None;
                }
                Ok(result)
            })
            .await
    }

    async fn scan_due(&self, limit: usize) -> Result<Vec<sea_orm::QueryResult>> {
        let cursor = self
            .maintenance_cursor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        self.authority.retry(|| async {
            let tx = self.authority.begin_read().await?;
            let now = timestamp(SharedContinuationDb::now(&tx).await?);
            // Canonical UTC text orders the normal timestamp domain. This is
            // only a hint scan; every mutation checks typed database time again.
            // https://www.postgresql.org/docs/current/sql-select.html
            let rows = tx.query_all(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT identity, revision, body, publication_state, lease_until, expires_at, purge_after
                 FROM gateway_shared_native_continuations
                 WHERE namespace = $1
                   AND ((publication_state <> $2 AND expires_at <= $3) OR purge_after <= $3)
                   AND ($4::text IS NULL OR identity > $4)
                 ORDER BY identity LIMIT $5",
                [self.authority.namespace().into(), PUBLICATION_EXPIRED.into(), now.into(),
                 cursor.clone().into(), i64::try_from(limit)?.into()],
            )).await?;
            tx.commit().await?;
            Ok(rows)
        }).await
    }

    async fn maintain_row(&self, hint: &Snapshot) -> Result<RowMaintenance> {
        self.authority
            .retry(|| async {
                let tx = self
                    .authority
                    .begin_cleanup(&hint.row.continuation_identity)
                    .await?;
                let Some(current) = self.lock_row(&tx, &hint.row.continuation_identity).await?
                else {
                    tx.commit().await?;
                    return Ok(RowMaintenance::Unchanged);
                };
                // Exact content comparison also fences a delete/rebind whose fresh
                // row starts again at revision zero.
                if current.revision != hint.revision || current.body != hint.body {
                    tx.commit().await?;
                    return Ok(RowMaintenance::Unchanged);
                }
                self.decrypt(&current.row)?;
                let now = SharedContinuationDb::now(&tx).await?;
                let outcome = if parse_timestamp(&current.row.purge_after)? <= now {
                    if self.purge(&tx, &current).await? {
                        RowMaintenance::Purged
                    } else {
                        RowMaintenance::Unchanged
                    }
                } else if current.row.publication_state != PUBLICATION_EXPIRED
                    && parse_timestamp(&current.row.expires_at)? <= now
                    && self.scrub(&tx, &current).await?
                {
                    RowMaintenance::Scrubbed
                } else {
                    RowMaintenance::Unchanged
                };
                tx.commit().await?;
                Ok(outcome)
            })
            .await
    }

    async fn scrub(&self, tx: &DatabaseTransaction, previous: &Snapshot) -> Result<bool> {
        let mut row = previous.row.clone();
        row.publication_state = PUBLICATION_EXPIRED.into();
        // An authenticated empty payload removes the native id while retaining
        // verifiable owner/generation/retention metadata until physical purge.
        self.seal(&mut row, &[])?;
        let body = serde_json::to_string(&row)?;
        ensure!(
            body.len() <= MAX_NATIVE_BYTES,
            "shared native record exceeds byte limit"
        );
        let revision = previous
            .revision
            .checked_add(1)
            .context("shared native revision exhausted")?;
        // The post-lock sample is not sufficient if the process pauses or its
        // database clock changes before this final conditional write.
        // https://www.postgresql.org/docs/current/queries-with.html#QUERIES-WITH-CTE-MATERIALIZATION
        let changed = tx
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "WITH decision_clock AS MATERIALIZED (SELECT clock_timestamp() AS t)
             UPDATE gateway_shared_native_continuations AS c
             SET revision = $1, body = $2, publication_state = $3
             FROM decision_clock AS n
             WHERE c.namespace = $4 AND c.identity = $5 AND c.revision = $6 AND c.body = $7
               AND c.expires_at::timestamptz <= n.t",
                [
                    revision.into(),
                    body.into(),
                    PUBLICATION_EXPIRED.into(),
                    self.authority.namespace().into(),
                    previous.row.continuation_identity.clone().into(),
                    previous.revision.into(),
                    previous.body.clone().into(),
                ],
            ))
            .await?;
        Ok(changed.rows_affected() == 1)
    }

    async fn purge(&self, tx: &DatabaseTransaction, previous: &Snapshot) -> Result<bool> {
        // Purge authority is separate from publication ownership and expiry.
        // https://www.postgresql.org/docs/current/sql-delete.html
        let deleted = tx
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "WITH decision_clock AS MATERIALIZED (SELECT clock_timestamp() AS t)
             DELETE FROM gateway_shared_native_continuations AS c USING decision_clock AS n
             WHERE c.namespace = $1 AND c.identity = $2 AND c.revision = $3 AND c.body = $4
               AND c.purge_after::timestamptz <= n.t",
                [
                    self.authority.namespace().into(),
                    previous.row.continuation_identity.clone().into(),
                    previous.revision.into(),
                    previous.body.clone().into(),
                ],
            ))
            .await?;
        Ok(deleted.rows_affected() == 1)
    }
}

enum RowMaintenance {
    Unchanged,
    Scrubbed,
    Purged,
}

#[cfg(test)]
#[path = "tests_shared_lifecycle.rs"]
mod tests;
