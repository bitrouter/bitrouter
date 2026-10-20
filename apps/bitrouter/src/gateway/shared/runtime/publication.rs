//! Materialize one published authority; process inputs never provide routes.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, ensure};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::super::authority::{Authority, Revision};
use super::super::credentials::Credentials;
use super::super::generations::{Generations, Input};
use super::health::Health;
use crate::config::gateway_state::GatewayStateConfig;
use crate::config::shared_process::Process;

async fn materialize(authority: &Authority, process: &Process) -> Result<Input> {
    let head = authority.head().await?;
    ensure!(
        head.config_generation > 0,
        "shared namespace has no published configuration"
    );
    let published = authority.configuration(&head).await?;
    let mut prepared = published.prepare()?;
    let keys = authority.keys(&head).await?;
    let auth = super::super::authentication::build(
        &prepared,
        &published.credentials,
        Credentials::new(authority.clone()),
    )?;
    prepared.config.database.url = process.database_url.clone();
    prepared.config.gateway_state = GatewayStateConfig::Shared(process.shared.clone());
    prepared.config.server.listen = process.server.listen.clone();
    prepared.config.server.log_level = process.server.log_level.clone();
    prepared.config.server.skip_auth = false;
    // require_known_pricing, policies and policy lock remain publication-owned.
    Ok(Input {
        revision: head.revision(),
        config: prepared.config,
        authority: authority.clone(),
        continuation_key: keys.continuation()?,
        correlation_key: keys.active_correlation()?,
        auth_appliers: auth,
        policies: prepared.policies,
        policy_lock: published.policy_lock,
    })
}

async fn build(
    authority: &Authority,
    process: &Process,
    generations: &Generations,
) -> Result<Revision> {
    let input = materialize(authority, process).await?;
    let revision = input.revision;
    generations.start_candidate(input)?;
    let mut candidate_owner = CandidatePublication {
        generations,
        published: false,
    };
    let deadline = Instant::now() + Duration::from_millis(process.shared.drain_timeout_ms);
    let candidate = generations.wait_candidate(deadline).await?;
    if !candidate.ready || authority.head().await?.revision() != revision {
        anyhow::bail!("shared candidate could not publish the current authority");
    }
    generations.publish_candidate()?;
    candidate_owner.published = true;
    Ok(revision)
}

struct CandidatePublication<'a> {
    generations: &'a Generations,
    published: bool,
}

impl Drop for CandidatePublication<'_> {
    fn drop(&mut self) {
        if !self.published && self.generations.discard_candidate().is_err() {
            tracing::error!("shared publication could not mark its candidate retiring");
        }
    }
}

pub(super) async fn initial(
    authority: &Authority,
    process: &Process,
    generations: &Generations,
    health: &Health,
) -> Result<()> {
    let revision = build(authority, process, generations).await?;
    health.authority(Some(revision));
    Ok(())
}

pub(super) async fn run(
    authority: Authority,
    process: Process,
    generations: Arc<Generations>,
    health: Arc<Health>,
    stop: CancellationToken,
) -> Result<()> {
    let interval = Duration::from_millis(process.shared.worker_interval_ms);
    loop {
        tokio::select! { biased; () = stop.cancelled() => return Ok(()), () = tokio::time::sleep(interval) => {} }
        let pass = async {
            match authority.head().await {
                Ok(head) => {
                    health.authority(Some(head.revision()));
                    if generations.serving().loaded_revision() != Some(head.revision())
                        && generations.can_build()
                    {
                        match build(&authority, &process, &generations).await {
                            Ok(revision) => health.authority(Some(revision)),
                            Err(_) => {
                                health.authority(None);
                                tracing::warn!("shared publication deferred");
                            }
                        }
                    }
                }
                Err(_) => {
                    health.authority(None);
                    tracing::warn!("shared authority check failed");
                }
            }
            Ok::<(), anyhow::Error>(())
        };
        // Cancellation releases only borrowed waiters. The manager retains any
        // candidate or retirement, including a late successful build result.
        tokio::select! { biased; () = stop.cancelled() => return Ok(()), result = pass => result? }
    }
}

pub(super) async fn retire(
    generations: Arc<Generations>,
    interval: Duration,
    budget: Duration,
    stop: CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! { biased; () = stop.cancelled() => return Ok(()), () = tokio::time::sleep(interval) => {} }
        // This separate worker cannot stall authority polling behind an old
        // response source. Each ordinary pass explicitly chooses a new attempt.
        let deadline = Instant::now() + budget;
        let waits = generations.retiring().into_iter().map(|id| {
            let generations = &generations;
            async move {
                let attempt = generations.begin_retirement(id, deadline)?;
                generations.retire(id, attempt).await
            }
        });
        let results = tokio::select! { biased;
            () = stop.cancelled() => return Ok(()),
            results = futures::future::join_all(waits) => results,
        };
        for result in results {
            if result.is_err() {
                tracing::warn!("shared generation retirement deferred");
            }
        }
    }
}
