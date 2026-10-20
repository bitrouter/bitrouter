//! Namespace-scoped native publication with database-owned time.
//!
//! These transactions contain only bounded local cryptography and SQL. Delivery,
//! provider calls and credential resolution belong outside their lock lifetime.

use anyhow::ensure;
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseTransaction, Statement};

use super::*;
use crate::gateway::shared::continuation::SharedContinuationDb;

const MAX_NATIVE_BYTES: usize = 2 * 1024 * 1024;
// Leave room for the authentication tag, base64 expansion and bounded metadata.
const MAX_NATIVE_PLAINTEXT_BYTES: usize = MAX_NATIVE_BYTES / 2;
const SHARED_IDENTITY_DOMAIN: &[u8] = b"bitrouter.shared-continuation.identity.v1";
const SHARED_OWNER_DOMAIN: &[u8] = b"bitrouter.shared-continuation.owner.v1";
const SHARED_TARGET_DOMAIN: &[u8] = b"bitrouter.shared-continuation.target.v1";

pub(super) struct SharedNative {
    authority: SharedContinuationDb,
    key: ContinuationKey,
}

pub(super) struct Bound {
    pub outcome: BindOutcome,
    pub row: continuation_entity::Model,
}

#[derive(Debug, thiserror::Error)]
pub(super) enum Rejection {
    #[error("shared native publication ownership changed")]
    Changed,
    #[error("shared native publication authority expired")]
    Expired,
    #[error("shared native payload exceeds byte limit")]
    PayloadTooLarge,
}

struct Snapshot {
    body: String,
    revision: i64,
    row: continuation_entity::Model,
}

impl SharedNative {
    pub(super) fn new(authority: SharedContinuationDb) -> Self {
        let key = authority.key();
        Self { authority, key }
    }

    fn identity(&self, owner: &str, request: &str) -> Result<String> {
        // JSON framing binds field boundaries, including embedded NUL bytes.
        let fields = serde_json::to_string(&(self.authority.namespace(), owner, request))?;
        Ok(format!(
            "shared-continuation-{}",
            hex::encode(hmac_bytes(
                &self.key.secret,
                SHARED_IDENTITY_DOMAIN,
                &[&fields]
            )?)
        ))
    }

    fn owner_identity(&self, owner: &str) -> Result<String> {
        let fields = serde_json::to_string(&(self.authority.namespace(), owner))?;
        Ok(format!(
            "shared-continuation-owner-{}",
            hex::encode(hmac_bytes(
                &self.key.secret,
                SHARED_OWNER_DOMAIN,
                &[&fields]
            )?)
        ))
    }

    /// Shared resolution must use this framing too; the local persisted target
    /// fingerprint intentionally retains its previous encoding.
    pub(super) fn target_fingerprint(
        &self,
        target: &RoutingTarget,
        authority: &ContinuationAuthority,
    ) -> Result<String> {
        let endpoint = target
            .api_base_override
            .as_deref()
            .unwrap_or(&target.api_base);
        let scheme = match authority.effective_scheme() {
            crate::gateway::model::target::AuthScheme::XApiKey => "x-api-key",
            crate::gateway::model::target::AuthScheme::Bearer => "bearer",
        };
        let fields = serde_json::to_string(&(
            self.authority.namespace(),
            &target.provider_name,
            target.account_label.as_deref().unwrap_or(""),
            target.api_protocol.to_string(),
            endpoint,
            scheme,
            hex::encode(authority.credential().proof_bytes()),
        ))?;
        Ok(format!(
            "shared-continuation-target-{}",
            hex::encode(hmac_bytes(
                &self.key.secret,
                SHARED_TARGET_DOMAIN,
                &[&fields]
            )?)
        ))
    }

    /// The random intent is allocated before SQL; authoritative times are not.
    pub(super) async fn bind(
        &self,
        owner: &str,
        request: &str,
        binding: &ContinuationBinding<'_>,
        generation: &str,
        state: &'static str,
        retention: TimeDelta,
    ) -> Result<Bound> {
        ensure!(
            matches!(state, PUBLICATION_PROVISIONAL | PUBLICATION_ACTIVE),
            "invalid initial shared publication state"
        );
        ensure!(
            !generation.is_empty() && generation.len() <= 128,
            "invalid shared publication generation"
        );
        ensure!(
            binding.provider_response_id.len() <= MAX_NATIVE_PLAINTEXT_BYTES,
            Rejection::PayloadTooLarge
        );
        let identity = self.identity(owner, request)?;
        let owner_identity = self.owner_identity(owner)?;
        let fingerprint = self.target_fingerprint(binding.target, binding.credential_authority)?;
        let payload = ContinuationPayload::current(
            binding.provider_response_id,
            binding.effective_model,
            binding.effective_effort,
            binding.causal_prefix_commitment,
        )?;
        let serialized = serde_json::to_vec(&payload)?;
        ensure!(
            serialized.len()
                <= MAX_NATIVE_PLAINTEXT_BYTES.saturating_sub(CONTINUATION_PAYLOAD_V4_MAGIC.len()),
            Rejection::PayloadTooLarge
        );
        let mut plaintext = CONTINUATION_PAYLOAD_V4_MAGIC.to_vec();
        plaintext.extend_from_slice(&serialized);
        self.authority
            .retry(|| async {
                let tx = self.authority.begin_bind(&identity).await?;
                let existing = self.lock_row(&tx, &identity).await?;
                let now = SharedContinuationDb::now(&tx).await?;
                if let Some(existing) = existing {
                    let row = existing.row;
                    let actual = decode_payload(self.decrypt(&row)?)?;
                    ensure!(
                        row.owner_identity == owner_identity
                            && row.target_fingerprint == fingerprint
                            && actual == payload,
                        "shared continuation identity has a different binding"
                    );
                    ensure!(parse_timestamp(&row.expires_at)? > now, Rejection::Expired);
                    let outcome = if row.publication_generation == generation
                        && row.publication_instance_id == self.authority.producer_id()
                        && row.publication_state == state
                    {
                        ensure!(
                            parse_timestamp(&row.publication_lease_until)? > now,
                            Rejection::Expired
                        );
                        BindOutcome::Inserted
                    } else if row.publication_state == PUBLICATION_ACTIVE {
                        BindOutcome::ExistingActive
                    } else {
                        BindOutcome::ExistingProvisional
                    };
                    tx.commit().await?;
                    return Ok(Bound { outcome, row });
                }
                let expires = now
                    .checked_add_signed(retention)
                    .context("shared native retention exceeds supported time")?;
                let purge = expires
                    .checked_add_signed(retention)
                    .context("shared native purge exceeds supported time")?;
                let mut row = continuation_entity::Model {
                    continuation_identity: identity.clone(),
                    owner_identity: owner_identity.clone(),
                    ciphertext: None,
                    nonce: None,
                    target_fingerprint: fingerprint.clone(),
                    key_id: self.key.key_id.clone(),
                    cipher_version: CIPHER_VERSION,
                    created_at: timestamp(now),
                    expires_at: timestamp(expires),
                    purge_after: timestamp(purge),
                    publication_state: state.into(),
                    publication_generation: generation.into(),
                    publication_instance_id: self.authority.producer_id().into(),
                    publication_lease_until: publication_lease_until(now)?,
                };
                self.seal(&mut row, &plaintext)?;
                self.insert(&tx, &row).await?;
                tx.commit().await?;
                Ok(Bound {
                    outcome: BindOutcome::Inserted,
                    row,
                })
            })
            .await
    }

    pub(super) async fn renew(
        &self,
        publication: &PendingPublication,
    ) -> Result<continuation_entity::Model> {
        ensure!(
            publication.phase == PendingPublicationPhase::Prepared,
            "only prepared publications can renew forward authority"
        );
        ensure!(
            publication.instance_id == self.authority.producer_id(),
            Rejection::Changed
        );
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
                // Authentication precedes ownership/expiry-based mutation.
                let plaintext = self.decrypt(&snapshot.row)?;
                ensure!(
                    publication_owns_row(publication, &snapshot.row),
                    Rejection::Changed
                );
                let now = SharedContinuationDb::now(&tx).await?;
                ensure!(
                    parse_timestamp(&snapshot.row.publication_lease_until)? > now
                        && parse_timestamp(&snapshot.row.expires_at)? > now,
                    Rejection::Expired
                );
                let mut row = snapshot.row.clone();
                if parse_timestamp(&row.publication_lease_until)?
                    <= now + TimeDelta::seconds(PUBLICATION_LEASE_SECONDS / 2)
                {
                    row.publication_lease_until = publication_lease_until(now)?;
                    self.seal(&mut row, &plaintext)?;
                    self.save_live(&tx, &snapshot, &row).await?;
                }
                tx.commit().await?;
                Ok(row)
            })
            .await
    }

    fn seal(&self, row: &mut continuation_entity::Model, plaintext: &[u8]) -> Result<()> {
        let aad = self.aad(row)?;
        let (ciphertext, nonce) = encrypt(&self.key, plaintext, &aad)?;
        row.ciphertext = Some(ciphertext);
        row.nonce = Some(nonce);
        Ok(())
    }

    fn decrypt(&self, row: &continuation_entity::Model) -> Result<Vec<u8>> {
        decrypt_authenticated(&self.key, row, &self.aad(row)?)
    }

    fn aad(&self, row: &continuation_entity::Model) -> Result<Vec<u8>> {
        let mut aad = b"bitrouter.shared-continuation.aead.v1\0".to_vec();
        let namespace = self.authority.namespace().as_bytes();
        aad.extend_from_slice(&u64::try_from(namespace.len())?.to_be_bytes());
        aad.extend_from_slice(namespace);
        aad.extend_from_slice(&row_aad(row));
        Ok(aad)
    }

    async fn lock_row(&self, tx: &DatabaseTransaction, identity: &str) -> Result<Option<Snapshot>> {
        // Advisory identity fencing covers absence; FOR UPDATE also ensures a
        // queued owner samples its clock after a real native-row lock is held.
        // https://www.postgresql.org/docs/current/explicit-locking.html#LOCKING-ROWS
        let record = tx
            .query_one(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT revision, body, publication_state, lease_until, expires_at, purge_after
                 FROM gateway_shared_native_continuations
                 WHERE namespace = $1 AND identity = $2 FOR UPDATE",
                [self.authority.namespace().into(), identity.into()],
            ))
            .await?;
        let Some(record) = record else {
            return Ok(None);
        };
        let body: String = record.try_get("", "body")?;
        ensure!(
            body.len() <= MAX_NATIVE_BYTES,
            "shared native record exceeds byte limit"
        );
        let row: continuation_entity::Model = serde_json::from_str(&body)?;
        let revision: i64 = record.try_get("", "revision")?;
        ensure!(revision >= 0, "negative shared native revision");
        ensure!(
            row.continuation_identity == identity
                && row.key_id == self.key.key_id
                && row.cipher_version == CIPHER_VERSION
                && row.publication_state == record.try_get::<String>("", "publication_state")?
                && row.publication_lease_until == record.try_get::<String>("", "lease_until")?
                && row.expires_at == record.try_get::<String>("", "expires_at")?
                && row.purge_after == record.try_get::<String>("", "purge_after")?,
            "shared native record metadata mismatch"
        );
        for value in [
            &row.created_at,
            &row.publication_lease_until,
            &row.expires_at,
            &row.purge_after,
        ] {
            ensure!(
                timestamp(parse_timestamp(value)?) == *value,
                "noncanonical native timestamp"
            );
        }
        Ok(Some(Snapshot {
            body,
            revision,
            row,
        }))
    }

    async fn insert(
        &self,
        tx: &DatabaseTransaction,
        row: &continuation_entity::Model,
    ) -> Result<()> {
        let body = serde_json::to_string(row)?;
        ensure!(
            body.len() <= MAX_NATIVE_BYTES,
            "shared native record exceeds byte limit"
        );
        // One volatile clock sample guards the final write after local sealing.
        // https://www.postgresql.org/docs/current/queries-with.html#QUERIES-WITH-CTE-MATERIALIZATION
        // https://www.postgresql.org/docs/current/sql-insert.html#SQL-ON-CONFLICT
        let inserted = tx.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "WITH decision_clock AS MATERIALIZED (SELECT clock_timestamp() AS t)
             INSERT INTO gateway_shared_native_continuations
                (namespace, identity, revision, body, publication_state, lease_until, expires_at, purge_after)
             SELECT $1, $2, 0, $3, $4, $5, $6, $7 FROM decision_clock
             WHERE $5::timestamptz > t AND $6::timestamptz > t
             ON CONFLICT (namespace, identity) DO NOTHING",
            [self.authority.namespace().into(), row.continuation_identity.clone().into(),
             body.into(), row.publication_state.clone().into(),
             row.publication_lease_until.clone().into(), row.expires_at.clone().into(),
             row.purge_after.clone().into()],
        )).await?;
        ensure!(
            inserted.rows_affected() == 1,
            "shared native insertion expired or conflicted"
        );
        Ok(())
    }

    async fn save_live(
        &self,
        tx: &DatabaseTransaction,
        previous: &Snapshot,
        row: &continuation_entity::Model,
    ) -> Result<()> {
        let revision = previous
            .revision
            .checked_add(1)
            .context("shared native revision exhausted")?;
        let body = serde_json::to_string(row)?;
        ensure!(
            body.len() <= MAX_NATIVE_BYTES,
            "shared native record exceeds byte limit"
        );
        // Lock-before-clock alone cannot cover a process paused during sealing.
        // The old lease, retention and replacement lease must all still be live
        // at this guarded write. Body equality preserves the complete snapshot.
        // https://www.postgresql.org/docs/current/queries-with.html#QUERIES-WITH-CTE-MATERIALIZATION
        let saved = tx
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "WITH decision_clock AS MATERIALIZED (SELECT clock_timestamp() AS t)
             UPDATE gateway_shared_native_continuations AS c
             SET revision = $1, body = $2, publication_state = $3,
                 lease_until = $4, expires_at = $5, purge_after = $6
             FROM decision_clock AS n
             WHERE c.namespace = $7 AND c.identity = $8 AND c.revision = $9 AND c.body = $10
               AND c.lease_until::timestamptz > n.t AND c.expires_at::timestamptz > n.t
               AND $4::timestamptz > n.t AND $5::timestamptz > n.t",
                [
                    revision.into(),
                    body.into(),
                    row.publication_state.clone().into(),
                    row.publication_lease_until.clone().into(),
                    row.expires_at.clone().into(),
                    row.purge_after.clone().into(),
                    self.authority.namespace().into(),
                    previous.row.continuation_identity.clone().into(),
                    previous.revision.into(),
                    previous.body.clone().into(),
                ],
            ))
            .await?;
        ensure!(saved.rows_affected() == 1, Rejection::Expired);
        Ok(())
    }
}

#[cfg(test)]
#[path = "tests_shared_clock.rs"]
mod tests;
