//! Application-owned advisory provider ranking; it grants no execution authority.

use bitrouter_ai::tenancy::{
    identity::CorrelationId,
    ordered::{
        CandidateManifestV1, OrderedCandidateSelectionV1, OrderedCapabilitiesV2,
        OrderedDispatchBindingV2,
    },
    validation::Validate,
};

use crate::{
    config::inference_routing::Policy,
    gateway::error::{BitrouterError, Result},
};

/// Success-conditioned latency for one common input/output bucket and response mode.
/// A trusted host must supply comparable measurements, never estimated zeroes.
#[derive(Clone, Copy)]
pub struct Latency {
    pub samples: u8,
    pub mean_millis: u64,
    pub age_millis: u32,
}

/// Nonsecret, bounded advisory state for one original candidate ordinal.
#[derive(Clone, Copy)]
pub struct Signal {
    pub ordinal: usize,
    pub outstanding_work: u64,
    pub capacity: u32,
    pub bookable: bool,
    pub affinity_millionths: u32,
    pub latency: Option<Latency>,
}

fn invalid() -> BitrouterError {
    BitrouterError::bad_request("invalid provider planning snapshot")
}

fn product(a: u128, b: u128) -> Result<u128> {
    a.checked_mul(b).ok_or_else(invalid)
}

/// Rank an operator-read manifest before explicit ordered v2 submission.
///
/// `signals` is the host's explicitly selected compatible subset for one fixed
/// model/root, using original manifest ordinals. Public model names alone do not
/// prove compatibility. No manifest access, credentials or capacity are acquired.
/// All original instance/revision bindings remain subject to server admission.
///
/// # Errors
/// Rejects invalid/stale bindings, unknown/duplicate ordinals, invalid signals or
/// a selection exceeding the negotiated whole-chain limit. Never truncates it.
pub fn rank_manifest(
    capabilities: &OrderedCapabilitiesV2,
    manifest: &CandidateManifestV1,
    correlation: CorrelationId,
    signals: &[Signal],
    work: u64,
    policy: Policy,
) -> Result<OrderedDispatchBindingV2> {
    capabilities.validate().map_err(|_| invalid())?;
    manifest.validate().map_err(|_| invalid())?;
    if capabilities.installation_id != manifest.installation_id
        || capabilities.authority_revision != manifest.authority_revision
        || manifest
            .runtime_observation
            .as_ref()
            .is_some_and(|observation| observation.instance_id != capabilities.instance_id)
        || signals.len() > usize::from(capabilities.attempt_limit)
        || signals.iter().any(|signal| {
            !manifest
                .candidates
                .iter()
                .any(|candidate| usize::from(candidate.candidate_index) == signal.ordinal)
        })
    {
        return Err(invalid());
    }
    let candidate_indices = rank(signals, work, policy)?
        .into_iter()
        .map(|ordinal| u16::try_from(ordinal).map_err(|_| invalid()))
        .collect::<Result<Vec<_>>>()?;
    capabilities
        .bind(
            manifest.execution_revision.clone(),
            correlation,
            OrderedCandidateSelectionV1 {
                schema_version: 1,
                admission_mode: manifest.admission_mode,
                route: manifest.route.clone(),
                candidate_indices,
                generation_defaults_digest: None,
            },
        )
        .map_err(|_| invalid())
}

// Exact rational pressure/score comparisons avoid rounding ties in normalized
// pressure. Common 4096 and 10-million denominators cancel in cross-products.
pub(crate) fn rank(signals: &[Signal], work: u64, policy: Policy) -> Result<Vec<usize>> {
    if signals.is_empty() || signals.len() > 4096 || !(256..=131072).contains(&work) {
        return Err(invalid());
    }
    let mut ordinals = std::collections::BTreeSet::new();
    let mut remaining = Vec::with_capacity(signals.len());
    let mut blocked = Vec::new();
    for signal in signals {
        if signal.ordinal >= 4096
            || signal.capacity == 0
            || signal.affinity_millionths > 1_000_000
            || !ordinals.insert(signal.ordinal)
        {
            return Err(invalid());
        }
        if signal.bookable {
            remaining.push(*signal);
        } else {
            blocked.push(signal.ordinal);
        }
    }
    let pressure = |s: &Signal| u128::from(s.outstanding_work) + u128::from(work);
    let score = |s: &Signal| {
        product(
            pressure(s),
            10_000_000
                - if policy == Policy::PrefixAffinity {
                    u128::from(s.affinity_millionths)
                } else {
                    0
                },
        )
    };
    let mut order = Vec::with_capacity(signals.len());
    while !remaining.is_empty() {
        let mut lowest = 0;
        for index in 1..remaining.len() {
            if product(
                pressure(&remaining[index]),
                u128::from(remaining[lowest].capacity),
            )? < product(
                pressure(&remaining[lowest]),
                u128::from(remaining[index].capacity),
            )? {
                lowest = index;
            }
        }
        let mut tied: Vec<usize> = Vec::new();
        for (index, signal) in remaining.iter().enumerate() {
            if product(
                product(pressure(signal), u128::from(remaining[lowest].capacity))?,
                10,
            )? > product(
                product(pressure(&remaining[lowest]), u128::from(signal.capacity))?,
                11,
            )? {
                continue;
            }
            if let Some(&best) = tied.first() {
                match product(score(signal)?, u128::from(remaining[best].capacity))?.cmp(&product(
                    score(&remaining[best])?,
                    u128::from(signal.capacity),
                )?) {
                    std::cmp::Ordering::Less => {
                        tied.clear();
                        tied.push(index);
                    }
                    std::cmp::Ordering::Equal => tied.push(index),
                    std::cmp::Ordering::Greater => {}
                }
            } else {
                tied.push(index);
            }
        }
        let comparable = tied.iter().all(|&index| {
            remaining[index]
                .latency
                .is_some_and(|l| (3..=32).contains(&l.samples) && l.age_millis < 300_000)
        });
        let chosen = tied
            .into_iter()
            .min_by_key(|&index| {
                let s = &remaining[index];
                (
                    if comparable {
                        s.latency.map_or(0, |l| l.mean_millis)
                    } else {
                        0
                    },
                    s.ordinal,
                )
            })
            .ok_or_else(invalid)?;
        order.push(remaining.remove(chosen).ordinal);
    }
    blocked.sort_unstable();
    order.extend(blocked);
    Ok(order)
}

#[cfg(test)]
mod tests;
