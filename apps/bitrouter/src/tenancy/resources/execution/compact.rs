//! Normalize opaque public identifiers at the private cache boundary.
//!
//! Public contract types intentionally expose text, not allocation internals.
//! Rebuild their String storage through `Box<str>` before weighing the view, so
//! each identifier's capacity equals its byte length without widening that API.
//! Allocator size classes/fragmentation remain outside the retained-byte budget.
//! <https://doc.rust-lang.org/std/primitive.str.html#method.into_string>

use std::sync::Arc;

use bitrouter_ai::tenancy::{
    configuration::{Configuration, ResourceRef},
    operation::ErrorCode,
};

use super::{Candidate, RetainedExecutionRevision, RetainedPolicy, RouteData};

type Result<T> = std::result::Result<T, ErrorCode>;

fn tight<T: Clone + Into<String> + TryFrom<String>>(value: &mut T) -> Result<()> {
    let text: String = value.clone().into();
    *value = text
        .into_boxed_str()
        .into_string()
        .try_into()
        .map_err(|_| ErrorCode::AuthorityUnavailable)?;
    Ok(())
}

fn reference(value: &mut ResourceRef) -> Result<()> {
    let ResourceRef {
        owner_kind: _,
        owner_id,
        resource_kind: _,
        resource_id,
        revision_id,
    } = value;
    tight(owner_id)?;
    tight(resource_id)?;
    tight(revision_id)
}

pub(super) fn identifiers(value: &mut RetainedExecutionRevision) -> Result<()> {
    let RetainedExecutionRevision {
        installation,
        tenant,
        revision,
        authority_revision: _,
        document,
        routes,
    } = value;
    tight(installation)?;
    tight(tenant)?;
    tight(revision)?;
    let Configuration {
        routes: roots,
        policy,
        generation_defaults: _,
        learning: _,
        limits: _,
    } = document;
    for root in roots {
        reference(root)?;
    }
    if let Some(policy) = policy {
        reference(policy)?;
    }
    for route in routes.values_mut() {
        // The decoder creates each RouteData exactly once, before publishing
        // any view. Failure means the normalization boundary was bypassed.
        let RouteData {
            reference: root,
            dependencies,
            selector: _,
            overrides: _,
            policy,
            chains: _,
            candidates,
        } = Arc::get_mut(route).ok_or(ErrorCode::AuthorityUnavailable)?;
        reference(root)?;
        for dependency in dependencies {
            reference(dependency)?;
        }
        if let Some(RetainedPolicy {
            reference: policy,
            name: _,
            document: _,
            routing_source: _,
        }) = policy
        {
            reference(policy)?;
        }
        for Candidate {
            target: _,
            projection: _,
            credential,
        } in candidates
        {
            reference(credential)?;
        }
    }
    Ok(())
}
