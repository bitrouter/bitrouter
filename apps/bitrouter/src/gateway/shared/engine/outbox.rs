//! Fenced at-least-once delivery; accounting never runs in a delivery worker.

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::GatewayTerminalEvent;

use super::{Engine, Outbox, OutboxLease, Rejection};
use crate::gateway::shared::store::{Kind, Store, Stored};

#[derive(Clone)]
pub(in crate::gateway::shared) struct DeliveryClaim {
    pub event: GatewayTerminalEvent,
    token: String,
}

impl Engine {
    pub async fn claim_outbox(&self, limit: u64, lease_ms: u64) -> Result<Vec<DeliveryClaim>> {
        ensure!(
            (100..=300_000).contains(&lease_ms),
            "invalid outbox lease duration"
        );
        let lease_ms = i64::try_from(lease_ms)?;
        let now = self
            .store()
            .bounded(async {
                let tx = self.store().begin().await?;
                let now = Store::now(&tx).await?;
                tx.commit().await?;
                Ok(now)
            })
            .await?;
        let candidates = self
            .store()
            .due::<Outbox>(Kind::Outbox, "pending", now, limit)
            .await?;
        let mut claims = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let token = uuid::Uuid::new_v4().to_string();
            let claim = self
                .store()
                .retry(|| async {
                    let tx = self.store().begin().await?;
                    let mut outbox: Stored<Outbox> = self
                        .store()
                        .lock(&tx, Kind::Outbox, candidate.key())
                        .await?;
                    let now = Store::now(&tx).await?;
                    if outbox.value.acknowledged_at_ms.is_some()
                        || !outbox.due_at_ms().is_some_and(|due| due <= now)
                        || outbox
                            .value
                            .lease
                            .as_ref()
                            .is_some_and(|lease| lease.expires_at_ms > now)
                    {
                        tx.commit().await?;
                        return Ok(None);
                    }
                    self.live_owner(&tx, false).await?;
                    let now = Store::now(&tx).await?;
                    let expires_at_ms = now
                        .checked_add(lease_ms)
                        .context("outbox lease clock overflow")?;
                    outbox.value.lease = Some(OutboxLease {
                        token: token.clone(),
                        expires_at_ms,
                    });
                    outbox.value.deliveries = outbox
                        .value
                        .deliveries
                        .checked_add(1)
                        .context("outbox delivery count overflow")?;
                    self.store()
                        .save(&tx, &mut outbox, "pending", Some(expires_at_ms), now)
                        .await?;
                    let claim = DeliveryClaim {
                        event: outbox.value.event.clone(),
                        token: token.clone(),
                    };
                    tx.commit().await?;
                    Ok(Some(claim))
                })
                .await?;
            if let Some(claim) = claim {
                claims.push(claim);
            }
        }
        Ok(claims)
    }

    pub async fn acknowledge_outbox(&self, claim: &DeliveryClaim) -> Result<()> {
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                let mut outbox: Stored<Outbox> = self
                    .store()
                    .lock(&tx, Kind::Outbox, &claim.event.event_id)
                    .await?;
                ensure!(
                    outbox.value.event == claim.event,
                    "outbox event content changed"
                );
                let lease = outbox
                    .value
                    .lease
                    .as_ref()
                    .context("outbox delivery lease is missing")?;
                ensure!(lease.token == claim.token, Rejection::AuthorityChanged);
                if outbox.value.acknowledged_at_ms.is_some() {
                    tx.commit().await?;
                    return Ok(());
                }
                let now = Store::now(&tx).await?;
                ensure!(lease.expires_at_ms > now, Rejection::AuthorityChanged);
                outbox.value.acknowledged_at_ms = Some(now);
                self.store()
                    .save(&tx, &mut outbox, "delivered", None, now)
                    .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
    }

    pub async fn retry_outbox(&self, claim: &DeliveryClaim, delay_ms: u64) -> Result<()> {
        ensure!(
            (100..=300_000).contains(&delay_ms),
            "invalid outbox retry delay"
        );
        let delay_ms = i64::try_from(delay_ms)?;
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                let mut outbox: Stored<Outbox> = self
                    .store()
                    .lock(&tx, Kind::Outbox, &claim.event.event_id)
                    .await?;
                let lease = outbox
                    .value
                    .lease
                    .as_ref()
                    .context("outbox delivery lease is missing")?;
                let now = Store::now(&tx).await?;
                ensure!(
                    lease.token == claim.token
                        && lease.expires_at_ms > now
                        && outbox.value.acknowledged_at_ms.is_none(),
                    Rejection::AuthorityChanged
                );
                outbox.value.lease = None;
                let due = now
                    .checked_add(delay_ms)
                    .context("outbox retry clock overflow")?;
                self.store()
                    .save(&tx, &mut outbox, "pending", Some(due), now)
                    .await?;
                tx.commit().await?;
                Ok(())
            })
            .await
    }
}
