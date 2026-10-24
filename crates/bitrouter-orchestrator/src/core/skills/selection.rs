//! Pure candidate policy shared by native Core and external delivery hosts.
//! Execution, consent and dispatch authority stay with each owner.

use super::{SkillCandidate, SkillConstraints, SkillSource};
use crate::core::protocol::{CoreError, ErrorCode};

pub fn select(
    candidates: &mut [SkillCandidate],
    constraints: &SkillConstraints,
) -> Result<(), CoreError> {
    constraints.validate()?;
    for candidate in candidates.iter_mut() {
        let selector = candidate.source.selector();
        candidate.selected =
            constraints.include.contains(&selector) && !constraints.exclude.contains(&selector);
        candidate.reason = if constraints.exclude.contains(&selector) {
            "explicitly_excluded"
        } else if candidate.selected {
            "explicitly_required"
        } else {
            "not_explicitly_selected"
        }
        .into();
    }
    for required in &constraints.include {
        if !candidates
            .iter()
            .any(|candidate| candidate.selected && candidate.source.selector() == *required)
        {
            return Err(CoreError::rejected(
                ErrorCode::ArtifactUnavailable,
                "required skill is missing or explicitly excluded",
            ));
        }
    }
    Ok(())
}

pub fn bound(
    candidates: &[SkillCandidate],
    max_selected: usize,
    instruction_limit_bytes: u64,
    size: impl Fn(&SkillSource) -> Result<u64, CoreError>,
) -> Result<(usize, u64), CoreError> {
    let mut count = 0;
    let mut bytes = 0_u64;
    for candidate in candidates.iter().filter(|candidate| candidate.selected) {
        let needed = size(&candidate.source)?;
        let reason = if count >= max_selected {
            Some("selected_skill_count_budget")
        } else if bytes.saturating_add(needed) > instruction_limit_bytes {
            Some("skill_instruction_budget")
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(CoreError::rejected(ErrorCode::ArtifactUnavailable, reason));
        } else {
            count += 1;
            bytes += needed;
        }
    }
    Ok((count, bytes))
}
