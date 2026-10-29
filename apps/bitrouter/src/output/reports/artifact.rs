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

#[cfg(test)]
mod tests {
    use bitrouter_ai::{artifact::DistributionProfile, control::status::StatusReport};

    use crate::output::{Format, Output};

    #[test]
    fn shared_profile_survives_build_info_and_status_rendering() -> anyhow::Result<()> {
        let mut identity = crate::artifact::current();
        identity.profile = DistributionProfile::GatewayShared;
        identity.features = vec!["db-postgres".into(), "multi-instance".into()];
        let json = Output::new(Format::Json).render_to_vec(&identity);
        let wire: serde_json::Value = serde_json::from_slice(&json)?;
        assert_eq!(wire["profile"], "gateway-shared");
        assert_eq!(
            wire["features"],
            serde_json::json!(["db-postgres", "multi-instance"])
        );
        let human = String::from_utf8(Output::new(Format::Human).render_to_vec(&identity))?;
        assert!(human.contains("gateway-shared"));

        let mut status = StatusReport::running(
            7,
            "127.0.0.1:4356".into(),
            0,
            Vec::new(),
            "/fixture/socket".into(),
            None,
        );
        status.installed_artifact = Some(identity.clone());
        status.daemon_artifact = Some(identity.clone());
        let value: serde_json::Value =
            serde_json::from_slice(&Output::new(Format::Json).render_to_vec(&status))?;
        assert_eq!(
            value["installed_artifact"],
            serde_json::to_value(&identity)?
        );
        assert_eq!(value["daemon_artifact"], serde_json::to_value(&identity)?);
        let human = String::from_utf8(Output::new(Format::Human).render_to_vec(&status))?;
        assert!(human.contains("installed profile") && human.contains("daemon profile"));
        assert!(human.contains("gateway-shared") && human.contains("db-postgres, multi-instance"));
        Ok(())
    }
}
