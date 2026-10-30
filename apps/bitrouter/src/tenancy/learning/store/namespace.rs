//! SQL partition data; only callers already borrowing H may use these helpers.
//! It is not a permission, cleanup acknowledgement or live execution authority.
use crate::{
    harness::trajectory::owned::Association,
    storage::scope::{KeyDomain, Metadata, MetadataRef, Provenance, Scope},
};
use anyhow::{Result, ensure};

pub(in crate::tenancy::learning) trait Namespace {
    fn scope(&self) -> &Scope;
    fn metadata(&self) -> Result<Metadata>;
    fn key(&self, domain: KeyDomain, parts: &[&str]) -> Result<String> {
        self.scope().physical_key(domain, "", parts)
    }
    fn validate(
        &self,
        installation: Option<&str>,
        tenant: Option<&str>,
        creator: Option<&str>,
    ) -> Result<()> {
        self.scope().validate_metadata(MetadataRef {
            installation,
            tenant,
            creator,
        })?;
        ensure!(
            self.metadata()?.creator.as_deref() == creator,
            "paid stable creator changed"
        );
        Ok(())
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

pub(in crate::tenancy::learning) struct Native {
    scope: Scope,
    provenance: Provenance,
}
impl Native {
    pub(in crate::tenancy::learning) fn binding(
        binding: &crate::tenancy::execution::session::Binding,
    ) -> Self {
        let (scope, provenance) = Scope::for_session(binding.authority());
        Self { scope, provenance }
    }
    pub(in crate::tenancy::learning) fn new(scope: Scope, provenance: Provenance) -> Self {
        Self { scope, provenance }
    }
}
impl Namespace for Native {
    fn scope(&self) -> &Scope {
        &self.scope
    }
    fn metadata(&self) -> Result<Metadata> {
        self.scope.metadata(&self.provenance)
    }
}

/// Either existing checked capture partition or native bootstrap partition data.
pub(in crate::tenancy::learning) enum Stored {
    Capture(Box<Association>),
    Native(Native),
}
impl Namespace for Stored {
    fn scope(&self) -> &Scope {
        match self {
            Self::Capture(a) => a.scope(),
            Self::Native(a) => a.scope(),
        }
    }
    fn metadata(&self) -> Result<Metadata> {
        match self {
            Self::Capture(a) => a.metadata(),
            Self::Native(a) => a.metadata(),
        }
    }
}
