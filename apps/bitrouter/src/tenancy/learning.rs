//! Explicit checked checkpoint assessment, sharing execution and transaction custody.
mod control;
pub(in crate::tenancy) mod entities;
pub(in crate::tenancy) mod history;
pub(crate) mod native;
pub(in crate::tenancy) mod owner;
pub(in crate::tenancy) mod queue;
pub(crate) mod recovery;
pub(in crate::tenancy) mod source;
pub(in crate::tenancy) mod store;

use super::capture::Binding;
use crate::{acp_trajectory::checkpoint::owned::Summary, core::evolution::judge};
use anyhow::{Context, Result};
use bitrouter_ai::tenancy::configuration::ResourceRef;
use std::sync::Arc;

pub(in crate::tenancy) struct Profile {
    pub route: ResourceRef,
    pub dependencies: Vec<ResourceRef>,
    pub maximum: u32,
    pub concurrent: u32,
}

pub(in crate::tenancy) async fn checkpoint(
    binding: Arc<Binding>,
    native: &str,
    summary: &Summary,
) -> Result<()> {
    binding
        .assessment_check()
        .map_err(|_| anyhow::anyhow!("assessment authority rejected"))?;
    let prepared = queue::prepare(binding.clone(), native, summary)
        .await
        .map_err(|_| anyhow::anyhow!("assessment preparation rejected"))?;
    let profile = binding.assessment().context("assessment profile absent")?;
    let lease = binding
        .service()
        .admission
        .learning(binding.authority().identity().1, profile.concurrent)
        .map_err(|_| anyhow::anyhow!("assessment capacity exhausted"))?;
    // A child never queues while retaining a host/controller or learning slot.
    let execution = binding
        .authority()
        .admit_assessment(source::Binding::Capture(binding.clone()), &prepared.route)
        .await
        .map_err(|_| anyhow::anyhow!("assessment execution admission rejected"))?;
    let receipt = execution.receipt_owner()?.clone();
    let owner = owner::Owner::new(
        source::Binding::Capture(binding.clone()),
        prepared.clone(),
        &receipt,
        lease,
        receipt.deadline,
    )?;
    receipt.attach_learning(owner.clone())?;
    let mut runtime = owner.runtime();
    if !queue::accept(owner.clone())
        .await
        .map_err(|_| anyhow::anyhow!("assessment acceptance rejected"))?
    {
        return Ok(());
    }
    let prompt = judge::bounded_prompt(
        prepared.route.resource_id.as_str(),
        &prepared.input,
        16 * 1024 * 1024,
    )?;
    let response = binding
        .service()
        .execute_admitted_native(
            prompt,
            http::HeaderMap::new(),
            Arc::new(control::Control::new(
                source::Binding::Capture(binding.clone()),
                execution.id().clone(),
            )),
            execution,
            None, // Assessment has no native Core model-step binding.
        )
        .await;
    let parsed = response.and_then(|response| {
        judge::parse(
            &response,
            prepared.route.resource_id.as_str(),
            &prepared.input,
        )
        .and_then(|output| {
            crate::harness::trajectory::owned::io::json(&output, 512 * 1024)?;
            Ok(output)
        })
        .map_err(|_| crate::gateway::error::BitrouterError::internal("assessment output rejected"))
    });
    if let Ok(output) = &parsed {
        runtime.output(output.clone());
        if owner.recovery.is_some() {
            queue::observe(owner.clone(), output.clone())
                .await
                .map_err(|_| anyhow::anyhow!("assessment result retention unconfirmed"))?;
        }
    }
    // Notify the existing Registry only after the complete parsed result or
    // failure is fixed. It retains the learning lease through the actor ACK.
    drop(runtime);
    owner
        .wait_terminal()
        .await
        .map_err(|_| anyhow::anyhow!("assessment completion unconfirmed"))?;
    parsed
        .map(|_| ())
        .map_err(|_| anyhow::anyhow!("assessment provider or rubric failed"))
}
