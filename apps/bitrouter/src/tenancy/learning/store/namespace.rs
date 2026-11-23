//! SQL partition data; only callers already borrowing H may use these helpers.
//! It is not a permission, cleanup acknowledgement or live execution authority.
use crate::{
    harness::trajectory::owned::Association,
    storage::scope::{KeyDomain, Metadata, Scope},
};
use anyhow::Result;

pub(in crate::tenancy::learning) trait Namespace {
    fn scope(&self) -> &Scope;
    fn metadata(&self) -> Result<Metadata>;
    fn key(&self, domain: KeyDomain, parts: &[&str]) -> Result<String> {
        self.scope().physical_key(domain, "", parts)
    }
}
impl Namespace for Association {
    fn scope(&self) -> &Scope {
        Association::scope(self)
    }
    fn metadata(&self) -> Result<Metadata> {
        Association::metadata(self)
    }
}

/// Checked capture partition data for bootstrap recovery.
pub(in crate::tenancy::learning) enum Stored {
    Capture(Box<Association>),
}
impl Namespace for Stored {
    fn scope(&self) -> &Scope {
        match self {
            Self::Capture(a) => a.scope(),
        }
    }
    fn metadata(&self) -> Result<Metadata> {
        match self {
            Self::Capture(a) => a.metadata(),
        }
    }
}
