//! Compile-time application identity, with no runtime discovery or global state.

use bitrouter_ai::artifact::{ArtifactIdentity, DistributionProfile};

/// Return the metadata generated for this particular application build.
pub fn current() -> ArtifactIdentity {
    // The build script emits a typed expression rather than JSON so this path
    // cannot fail or require a parser, environment lookup, filesystem or Git.
    include!(concat!(env!("OUT_DIR"), "/artifact_generated.rs"))
}
