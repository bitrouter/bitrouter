//! Retained-profile qualification and bounded actor-owned publication preflight.
use anyhow::{Context, Result, ensure};
pub(crate) fn guarded(
    config: &crate::config::Config,
    document: &crate::policy_lock::PolicyLock,
    selected: &str,
) -> Result<bool> {
    let guarded = retained_guarded(config, document, selected)?;
    ensure!(
        !guarded || cfg!(feature = "core"),
        "owned trajectory requires core"
    );
    Ok(guarded)
}

/// Compiler-v1 composition checks are independent of the reading binary's
/// features. Execution qualification must additionally call `guarded`.
pub(crate) fn retained_guarded(
    config: &crate::config::Config,
    document: &crate::policy_lock::PolicyLock,
    selected: &str,
) -> Result<bool> {
    let policy = document
        .policies
        .get(selected)
        .context("selected retained policy missing")?;
    let active = policy
        .optimization
        .as_ref()
        .and_then(|value| value.active.as_ref());
    let Some(guard) = &policy.progress_guard else {
        ensure!(
            active.is_none(),
            "owned experiment requires the retained progress guard"
        );
        return Ok(false);
    };
    ensure!(
        config.trajectory.enabled,
        "selected guard requires retained trajectory enablement"
    );
    if let Some(active) = active {
        ensure!(
            guard.protected_tiers.contains(&active.champion_tier)
                && guard.protected_tiers.contains(&active.challenger_tier),
            "both owned experiment arms must be protected"
        );
    }
    ensure!(
        guard.max_episode_cost_micro_usd.is_none(),
        "owned trajectory cost guard is not qualified"
    );
    let defaults = crate::config::TrajectoryConfig::default();
    ensure!(
        config.trajectory.retention_days == defaults.retention_days
            && config.trajectory.outbox_batch_size == defaults.outbox_batch_size,
        "owned trajectory retention/publication profile is not qualified"
    );
    Ok(true)
}

/// Owned publication has no asynchronous worker. An undelivered row is not a
/// recoverable queue: the complete terminal transaction was required atomically.
/// This bounded projection runs under existing actor bootstrap before any write.
pub(super) async fn validate_startup(
    db: &impl sea_orm::ConnectionTrait,
    installation: &bitrouter_ai::tenancy::identity::InstallationId,
    control: &super::transaction::Control,
) -> Result<()> {
    use sea_orm::sea_query::{Alias, Condition, Expr, Query};
    let column = |name| Expr::col(Alias::new(name));
    let incomplete = Condition::any()
        .add(column("tenant_id").is_null())
        .add(column("creator_principal_id").is_null())
        .add(column("logical_identity_json").is_null())
        .add(column("delivered_at").is_null())
        .add(column("attempts").ne(1))
        .add(column("delivered_at").ne(column("created_at")));
    let invalid = Condition::any()
        .add(
            Condition::all()
                .add(column("installation_id").eq(installation.as_str()))
                .add(incomplete),
        )
        .add(
            Condition::all()
                .add(column("installation_id").is_null())
                .add(
                    Condition::any()
                        .add(column("tenant_id").is_not_null())
                        .add(column("creator_principal_id").is_not_null())
                        .add(column("logical_identity_json").is_not_null()),
                ),
        );
    let query = Query::select()
        .expr(Expr::val(1))
        .from(Alias::new("trajectory_outbox"))
        .cond_where(invalid)
        .limit(1)
        .to_owned();
    control.check_read()?;
    let invalid = db
        .query_one(db.get_database_backend().build(&query))
        .await?;
    control.check_read()?;
    ensure!(
        invalid.is_none(),
        "owned trajectory publication frontier is inconsistent"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn document() -> crate::policy_lock::PolicyLock {
        let mut document = crate::policy_lock::PolicyLock::default();
        document.policies.insert(
            "selected".into(),
            crate::policy_lock::PolicyDefinition {
                tiers: [(
                    "safe".into(),
                    bitrouter_ai::routing::PolicyModelTarget::model_only("provider:model".into()),
                )]
                .into(),
                default_tier: Some("safe".into()),
                progress_guard: Some(crate::trajectory::guard::ProgressGuardPolicy {
                    escalation_tier: "safe".into(),
                    protected_tiers: ["safe".into()].into(),
                    max_consecutive_unprotected: None,
                    max_same_projection_unprotected: None,
                    max_recovery_count: None,
                    max_episode_requests: Some(2),
                    max_episode_elapsed_ms: None,
                    max_episode_cost_micro_usd: None,
                    hold_for_requests: 1,
                    incomplete_history: crate::trajectory::guard::IncompleteHistoryAction::Observe,
                }),
                ..Default::default()
            },
        );
        document
    }
    #[test]
    fn selected_profile_rejects_unqualified_cost_retention_and_feature_without_affecting_inert_guard()
    -> Result<()> {
        let config = crate::config::parse_resolved(
            "inherit_defaults: false\ntrajectory:\n  enabled: true\n",
        )?;
        let mut document = document();
        ensure!(retained_guarded(&config, &document, "selected")?);
        ensure!(guarded(&config, &document, "selected").is_ok() == cfg!(feature = "core"));
        document
            .policies
            .get_mut("selected")
            .and_then(|p| p.progress_guard.as_mut())
            .context("guard")?
            .max_episode_cost_micro_usd = Some(1);
        ensure!(guarded(&config, &document, "selected").is_err());
        ensure!(retained_guarded(&config, &document, "selected").is_err());
        document.policies.insert(
            "unguarded".into(),
            crate::policy_lock::PolicyDefinition::default(),
        );
        ensure!(!guarded(&config, &document, "unguarded")?);
        ensure!(!retained_guarded(&config, &document, "unguarded")?);
        document
            .policies
            .get_mut("selected")
            .and_then(|p| p.progress_guard.as_mut())
            .context("guard")?
            .max_episode_cost_micro_usd = None;
        let mut changed = config.clone();
        changed.trajectory.retention_days += 1;
        ensure!(guarded(&changed, &document, "selected").is_err());
        ensure!(retained_guarded(&changed, &document, "selected").is_err());
        changed = config.clone();
        changed.trajectory.outbox_batch_size += 1;
        ensure!(guarded(&changed, &document, "selected").is_err());
        ensure!(retained_guarded(&changed, &document, "selected").is_err());
        changed = config;
        changed.trajectory.enabled = false;
        ensure!(guarded(&changed, &document, "selected").is_err());
        ensure!(retained_guarded(&changed, &document, "selected").is_err());
        Ok(())
    }
}
