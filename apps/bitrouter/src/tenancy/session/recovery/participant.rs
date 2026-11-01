//! Participants borrow the existing supervised native transaction.
use super::*;
use crate::managed_store::owned::{self, Loaded};
use crate::tenancy::{
    actor::Worker,
    execution::session::{Authority, Binding},
    receipts::store,
};
use anyhow::Context;
use sea_orm::DatabaseTransaction;

impl Worker {
    fn native_inventory_tenants(&self) -> Result<Vec<bitrouter_ai::tenancy::identity::TenantId>> {
        let snapshot = self
            .snapshot
            .read()
            .map_err(|_| anyhow::anyhow!("native inventory unavailable"))?;
        ensure!(snapshot.tenants.len() <= 32, "native inventory bound");
        Ok(snapshot.tenants.keys().cloned().collect())
    }

    pub(in crate::tenancy) async fn prepare_stopped_native(
        &self,
        tx: &DatabaseTransaction,
        authority: &Arc<Authority>,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<Option<Loaded>> {
        if self.startup.is_none() {
            return owned::load_in(tx, authority, check).await;
        }
        let header = owned::recovery_header(tx, authority, check).await?;
        let Some(metadata) = header.filter(|m| m.phase != Phase::Released) else {
            return owned::load_in(tx, authority, check).await;
        };
        metadata.validate()?;
        if !metadata.eligible || !authority.recovery_profile() || metadata.phase == Phase::Opening {
            return Err(crate::tenancy::session::Rejected(
                bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch,
            )
            .into());
        }
        self.startup
            .as_ref()
            .context("recovery origin absent")?
            .excludes(&self.proof, &metadata.origin)?;
        ensure!(
            metadata.profile_digest.as_deref()
                == Some(
                    authority
                        .recovery_profile_digest()
                        .map_err(anyhow::Error::msg)?
                        .as_str()
                ),
            "retained recovery profile changed"
        );
        let budget = Arc::new(Budget::default());
        let mut loaded = owned::load_bounded_in(tx, authority, check, Some(&budget))
            .await?
            .context("native predecessor absent")?;
        ensure!(
            loaded.metadata.as_ref() == Some(&metadata),
            "native provenance changed during read"
        );
        let value: serde_json::Value = serde_json::from_slice(&loaded.journal)?;
        let grant =
            serde_json::from_value(value.get("grant").context("native grant missing")?.clone())?;
        drop(value);
        let binding = authority
            .bind(loaded.descriptor.clone(), grant)
            .map_err(|_| anyhow::anyhow!("native source binding rejected"))?;
        let source = crate::harness::resources::managed::store::recovery::validate(
            &loaded.journal,
            &binding,
            &budget,
        )?;
        self.resources
            .recovery_static_credentials(tx, authority.dependencies(), &budget, check)
            .await?;
        owned::recovery_exclusion(
            tx,
            authority,
            &self.native_inventory_tenants()?,
            &budget,
            check,
        )
        .await?;
        let at: bitrouter_ai::tenancy::identity::Timestamp = chrono::Utc::now()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            .try_into()?;
        let mut receipts = Vec::new();
        for seed in &source.seeds {
            check()?;
            ensure!(
                seed.matches(&loaded.descriptor, &seed.execution),
                "native seed scope mismatch"
            );
            let saved = store::recovery_lookup(
                tx,
                &seed.installation,
                &seed.tenant,
                &seed.execution,
                &budget,
            )
            .await?;
            check()?;
            let (candidate, missing) = match saved {
                Some(candidate) => {
                    seed.validate_receipt(&candidate.receipt)?;
                    ensure!(
                        !candidate.has_auxiliary(),
                        "auxiliary work has no recovery proof"
                    );
                    (candidate, false)
                }
                None => {
                    seed.check(&loaded.descriptor, &metadata.origin)?;
                    let candidate = seed.unknown(&at)?;
                    budget.charge(
                        0,
                        usize::try_from(candidate.bytes() + candidate.auxiliary_bytes())?
                            .checked_mul(3)
                            .context("recovery receipt bound")?,
                    )?;
                    (candidate, true)
                }
            };
            receipts.push((Arc::new(candidate), missing));
        }
        let workspace_grant = metadata
            .workspace_grant
            .clone()
            .unwrap_or_else(|| source.grant.clone());
        ensure!(
            workspace_grant.session_id == source.grant.session_id
                && workspace_grant.harness_id == source.grant.harness_id
                && workspace_grant.execution_epoch <= source.grant.execution_epoch,
            "recovery workspace predecessor mismatch"
        );
        budget.charge(0, loaded.journal.len())?;
        let checked = Arc::new(CheckedRecovery {
            #[cfg(test)]
            observation: Default::default(),
            expected: loaded.clone(),
            budget,
            seeds: source.seeds,
            steps: source.steps,
            pending_steps: source.pending_steps,
            workspace: source.workspace,
            source_grant: source.grant,
            workspace_grant,
            receipts,
            committed: std::sync::atomic::AtomicBool::new(false),
            retired: std::sync::atomic::AtomicBool::new(false),
            reservation: Mutex::new(None),
        });
        crate::harness::resources::managed::store::recovery::validate_terminal_links(
            &loaded.journal,
            &checked,
        )?;
        loaded.recovery = Some(checked);
        Ok(Some(loaded))
    }

    pub(in crate::tenancy) async fn write_stopped_native(
        &self,
        tx: &DatabaseTransaction,
        binding: &Arc<Binding>,
        checked: &Arc<CheckedRecovery>,
        proposal: (u64, &[u8], &Metadata),
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        let (expected, bytes, metadata) = proposal;
        check()?;
        ensure!(
            !checked.committed()
                && expected == checked.expected.revision
                && binding.descriptor() == &checked.expected.descriptor,
            "native recovery participant mismatch"
        );
        let previous = checked
            .expected
            .metadata
            .as_ref()
            .context("native origin absent")?;
        self.startup
            .as_ref()
            .context("recovery origin absent")?
            .excludes(&self.proof, &previous.origin)?;
        ensure!(
            metadata.origin
                == self
                    .startup
                    .as_ref()
                    .context("recovery origin absent")?
                    .stamp(&self.proof)?
                && metadata.phase == Phase::Retiring
                && metadata.eligible
                && metadata.profile_digest == previous.profile_digest
                && metadata.workspace_grant.as_ref() == Some(checked.workspace_predecessor()),
            "native replacement provenance mismatch"
        );
        let current = owned::load_bounded_in(tx, binding.authority(), check, Some(&checked.budget))
            .await?
            .context("native predecessor absent")?;
        ensure!(
            current.revision == expected
                && current.row_digest == checked.expected.row_digest
                && current.descriptor == checked.expected.descriptor
                && current.journal == checked.expected.journal,
            "native predecessor changed"
        );
        owned::recovery_exclusion(
            tx,
            binding.authority(),
            &self.native_inventory_tenants()?,
            &checked.budget,
            check,
        )
        .await?;
        let mut reserve_bytes = 0u64;
        let mut entries = Vec::new();
        for (seed, (candidate, missing)) in checked.seeds.iter().zip(&checked.receipts) {
            check()?;
            let saved = store::recovery_lookup(
                tx,
                &seed.installation,
                &seed.tenant,
                &seed.execution,
                &checked.budget,
            )
            .await?;
            check()?;
            match (saved, missing) {
                (None, true) => {
                    reserve_bytes = reserve_bytes
                        .checked_add(candidate.bytes() + candidate.auxiliary_bytes())
                        .context("native repair capacity overflow")?;
                    entries.push((
                        seed.execution.clone(),
                        chrono::DateTime::parse_from_rfc3339(
                            candidate.receipt.recorded_at.as_str(),
                        )?
                        .with_timezone(&chrono::Utc),
                    ));
                }
                (Some(saved), false) => {
                    seed.validate_receipt(&saved.receipt)?;
                    ensure!(
                        saved.digest == candidate.digest && !saved.has_auxiliary(),
                        "native receipt changed"
                    );
                }
                _ => anyhow::bail!("native receipt frontier changed"),
            }
        }
        crate::harness::resources::managed::store::recovery::validate_replacement(
            &checked.expected.journal,
            bytes,
            binding,
            checked,
        )?;
        let free = crate::tenancy::receipts::capacity::free_space(
            self.owner.as_ref().and_then(|o| o.path.parent()),
        )?;
        self.admission.receipt_probe(free);
        {
            let mut reservation = checked
                .reservation
                .lock()
                .map_err(|_| anyhow::anyhow!("recovery reservation unavailable"))?;
            ensure!(reservation.is_none(), "native repair already attempted");
            *reservation = Some(
                self.admission
                    .reserve_repair(&binding.descriptor().tenant, reserve_bytes, entries)
                    .map_err(crate::tenancy::session::Rejected)?,
            );
        }
        checked.budget.charge(
            0,
            bytes
                .len()
                .checked_mul(2)
                .context("native replacement bound")?,
        )?;
        for (candidate, missing) in &checked.receipts {
            if *missing {
                check()?;
                #[cfg(test)]
                let observation = self.proof.receipt_started_for_test(candidate);
                let inserted = store::insert(tx, candidate).await;
                #[cfg(test)]
                {
                    use crate::tenancy::transaction::receipt_observation::Step;
                    self.proof
                        .receipt_result_for_test(observation, Step::Store, &inserted);
                    if let Some(index) = observation {
                        // The observer is optional, bounded and reports only actual
                        // SQL outcomes. It cannot affect the production branch.
                        if checked.observation.set(index).is_err() {
                            tracing::debug!("native repair observation already set");
                        }
                    }
                    if let Ok(value) = &inserted {
                        self.proof.receipt_inserted_for_test(observation, *value);
                    }
                }
                ensure!(inserted?, "native receipt insert frontier changed");
                check()?;
            }
        }
        let result = owned::save_in(tx, binding, expected, bytes, Some(metadata), check).await;
        #[cfg(test)]
        self.proof.receipt_result_for_test(
            checked.observation.get().copied(),
            crate::tenancy::transaction::receipt_observation::Step::NativeCheckpoint,
            &result,
        );
        result
    }
}
impl CheckedRecovery {
    pub(in crate::tenancy) async fn validate_terminals(
        &self,
        tx: &DatabaseTransaction,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<()> {
        ensure!(
            self.committed(),
            "native recovery COMMIT remains unconfirmed"
        );
        for (seed, (candidate, _)) in self.seeds.iter().zip(&self.receipts) {
            check()?;
            let stored = store::recovery_lookup(
                tx,
                &seed.installation,
                &seed.tenant,
                &seed.execution,
                &self.budget,
            )
            .await?
            .context("native recovery terminal missing")?;
            check()?;
            seed.validate_receipt(&stored.receipt)?;
            ensure!(
                stored.digest == candidate.digest && !stored.has_auxiliary(),
                "native recovery terminal changed"
            );
        }
        Ok(())
    }

    pub(in crate::tenancy) fn settled(&self, committed: bool) -> Result<()> {
        if committed {
            self.committed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let mut reservation = self
            .reservation
            .lock()
            .map_err(|_| anyhow::anyhow!("recovery reservation unavailable"))?;
        if let Some(reservation) = reservation.as_mut() {
            reservation
                .finish(committed)
                .map_err(|_| anyhow::anyhow!("recovery accounting failed"))?;
        }
        Ok(())
    }
}
