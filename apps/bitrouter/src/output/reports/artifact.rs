//! Local executable identity without opening configuration or service state.

use bitrouter_ai::artifact::ArtifactIdentity;

use crate::output::{CliReport, human::Human};

impl CliReport for ArtifactIdentity {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.field("version", &self.version)?;
        h.field("profile", self.profile.as_str())?;
        h.field("target", &self.target)?;
        h.field("features", self.features.join(", "))?;
        h.field(
            "source revision",
            self.source_revision.as_deref().unwrap_or("unknown"),
        )?;
        h.field("handoff build", &self.handoff_build_id)
    }
}
