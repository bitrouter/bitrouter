//! Pairing mechanism only; no execution or source authority.
use super::*;
use crate::tenancy::receipts::provider_technical::Capture;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::provider_technical_facts_v2::HopProfile;

#[test]
fn fork_slots_keep_actual_hop_identity_and_same_context_overlap_loses_only_observer() -> Result<()>
{
    let owner = Arc::new(Capture::default());
    let other = Arc::new(Capture::default());
    let first = owner
        .begin_hop(HopProfile::CoreJsonHopCallbacksV1)
        .context("first hop")?;
    let second = owner
        .begin_hop(HopProfile::CoreJsonHopCallbacksV1)
        .context("second hop")?;
    let root = ObserverSlot::default();
    let fork = ObserverSlot::default();
    root.bind(Some(&first));
    fork.bind(Some(&second));
    ensure!(
        root.get(&owner)
            .is_some_and(|hop| Arc::ptr_eq(&hop, &first))
    );
    ensure!(
        fork.get(&owner)
            .is_some_and(|hop| Arc::ptr_eq(&hop, &second))
    );
    ensure!(root.get(&other).is_none());
    first.observer_start();
    root.bind(Some(&second));
    ensure!(root.get(&owner).is_none() && owner.healthy());
    Ok(())
}
