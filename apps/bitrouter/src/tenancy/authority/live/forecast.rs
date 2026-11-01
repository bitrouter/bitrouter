//! Borrowed per-record charge deltas, evaluated before any view is cloned.

use super::{Change, ErrorCode, Metadata, Result, View, memory};

/// The staging proof allows only old and final versions of each logical record.
/// Reject intermediate versions without allocating a second key registry.
pub(super) fn unique(changes: &[Change]) -> Result<()> {
    for (index, change) in changes.iter().enumerate() {
        if changes.iter().take(index).any(|old| same_key(old, change)) {
            return Err(ErrorCode::AuthorityUnavailable);
        }
    }
    Ok(())
}

fn same_key(left: &Change, right: &Change) -> bool {
    match (left, right) {
        (Change::Tenant(a), Change::Tenant(b)) => a.id == b.id,
        (Change::Principal(a), Change::Principal(b)) => a.id == b.id,
        (Change::Binding(a), Change::Binding(b)) => a.tenant == b.tenant && a.id == b.id,
        (Change::Credential(a), Change::Credential(b)) => a.id == b.id,
        (Change::Activation { tenant: a, .. }, Change::Activation { tenant: b, .. }) => a == b,
        (
            Change::ResourceLifecycle { record: a, .. },
            Change::ResourceLifecycle { record: b, .. },
        ) => a.reference == b.reference,
        // Immutable payloads are not copied into live views.
        _ => false,
    }
}

/// None means the view only needs a new publication stamp. Changes must first
/// pass `unique`; then each subtraction refers to a distinct admitted record.
pub(super) fn bytes(
    view: &View,
    changes: &[Change],
    oauth: Option<(&Metadata, &Metadata)>,
) -> Result<Option<usize>> {
    let state = &view.facts;
    let mut removed = 0;
    let mut added = 0;
    let mut affected = false;
    for change in changes {
        let delta = match change {
            Change::Tenant(value) => state
                .tenants
                .get(&value.id)
                .map(|old| -> Result<_> {
                    Ok((memory::tenant(old, false)?, memory::tenant(value, true)?))
                })
                .transpose()?,
            Change::Principal(value) => state
                .principals
                .get(&value.id)
                .map(|old| -> Result<_> {
                    Ok((
                        memory::principal(old, false)?,
                        memory::principal(value, true)?,
                    ))
                })
                .transpose()?,
            Change::Binding(value) => state
                .bindings
                .iter()
                .find(|(key, _)| key.0 == value.tenant && key.1 == value.id)
                .map(|(_, old)| -> Result<_> {
                    Ok((memory::binding(old, false)?, memory::binding(value, true)?))
                })
                .transpose()?,
            Change::Credential(value) => state
                .credentials
                .get(&value.id)
                .map(|old| -> Result<_> {
                    Ok((
                        memory::credential(old, false)?,
                        memory::credential(value, true)?,
                    ))
                })
                .transpose()?,
            Change::Activation {
                tenant, revision, ..
            } if state.tenants.contains_key(tenant) => {
                Some(match state.active_configurations.get(tenant) {
                    Some(old) => (old.as_str().len(), revision.as_str().len()),
                    None => (0, memory::activation(tenant, revision)?),
                })
            }
            Change::ResourceLifecycle { record, .. }
                if state.resource_lifecycle.loaded(&record.reference) =>
            {
                // Each loaded reference already reserves both map shapes and
                // the maximum identity payload, including a proven absence.
                Some((0, 0))
            }
            _ => None,
        };
        if let Some((old, new)) = delta {
            affected = true;
            memory::add(&mut removed, old)?;
            memory::add(&mut added, new)?;
        }
    }
    if let Some((old, new)) = oauth {
        for fact in &view.oauth {
            if fact.slot == new.slot {
                if fact.metadata.as_ref().is_some_and(|current| current != old) {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                affected = true;
                memory::add(
                    &mut removed,
                    memory::oauth_metadata(fact.metadata.as_ref(), false)?,
                )?;
                memory::add(&mut added, memory::oauth_metadata(Some(new), true)?)?;
            }
        }
    }
    if !affected {
        return Ok(None);
    }
    // Subtract all replaced records first. A growing record before a shrinking
    // one must not cause an artificial prefix overflow or capacity rejection.
    view.bytes
        .checked_sub(removed)
        .and_then(|bytes| bytes.checked_add(added))
        .map(Some)
        .ok_or(ErrorCode::CapacityExceeded)
}
