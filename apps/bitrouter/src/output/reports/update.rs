//! Report for the `bro update` self-updater.
//!
//! Every terminal outcome of `update` — a package-manager delegation, a
//! `--check` probe, a declined confirmation, a no-op, or a completed swap —
//! is one [`UpdateReport`]. Interactive prompts and progress are diagnostics
//! and go to stderr; this is the single result value on stdout.

use bitrouter_ai::artifact::ArtifactIdentity;
use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;

/// Result of `bro update`. `status` is the discriminant; the remaining
/// fields are populated only for the outcomes they belong to.
#[derive(Debug, Serialize)]
pub struct UpdateReport {
    /// `delegated` | `checked` | `aborted` | `unchanged` | `updated`.
    pub status: &'static str,
    /// The version of the currently-running binary.
    pub current_version: String,
    /// Identity of the invoking executable, including its distribution profile.
    /// This describes the old process even after a successful binary swap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_artifact: Option<ArtifactIdentity>,
    /// `checked`: whether a newer release exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_available: Option<bool>,
    /// The version this outcome refers to: the available release (`checked`)
    /// or the freshly-installed one (`updated`). Absent when unknown / N/A.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_version: Option<String>,
    /// Identity of the installed executable after independent verification.
    /// Never inferred from the old process or merely from a release version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_artifact: Option<ArtifactIdentity>,
    /// `updated`: why the installed executable's identity could not be verified.
    /// The installer completed, but automatic daemon handoff is unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_verification_error: Option<String>,
    /// `delegated`: how the binary was installed (`homebrew` | `cargo` |
    /// `npm` | `unknown`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_method: Option<&'static str>,
    /// `delegated`: the exact command to run to upgrade out-of-band.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade_command: Option<String>,
    /// Why self-update was refused or a matching release channel is unavailable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual_reason: Option<String>,
    /// `updated`: `restarted`, `compatible`, or `deferred`. Absent when no
    /// daemon runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon: Option<&'static str>,
    /// Why an installed binary could not take over the running daemon.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_reason: Option<String>,
    /// Verified state of the daemon after an update attempt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_listen: Option<String>,
}

impl UpdateReport {
    /// Attach the invoking binary's identity without changing outcome semantics.
    pub fn with_current_artifact(mut self, artifact: ArtifactIdentity) -> Self {
        self.current_artifact = Some(artifact);
        self
    }

    /// No cargo-dist receipt: tell the user the package-manager command instead
    /// of clobbering a managed install.
    pub fn delegated(
        current_version: String,
        install_method: &'static str,
        upgrade_command: String,
    ) -> Self {
        Self {
            status: "delegated",
            current_version,
            current_artifact: None,
            update_available: None,
            target_version: None,
            target_artifact: None,
            target_verification_error: None,
            install_method: Some(install_method),
            upgrade_command: Some(upgrade_command),
            manual_reason: None,
            daemon: None,
            daemon_reason: None,
            daemon_pid: None,
            daemon_version: None,
            daemon_instance_id: None,
            daemon_listen: None,
        }
    }

    /// `--check`: report whether a newer release exists, and which (when known).
    pub fn checked(
        current_version: String,
        available: bool,
        target_version: Option<String>,
    ) -> Self {
        Self {
            status: "checked",
            current_version,
            current_artifact: None,
            update_available: Some(available),
            target_version,
            target_artifact: None,
            target_verification_error: None,
            install_method: None,
            upgrade_command: None,
            manual_reason: None,
            daemon: None,
            daemon_reason: None,
            daemon_pid: None,
            daemon_version: None,
            daemon_instance_id: None,
            daemon_listen: None,
        }
    }

    /// The user declined the confirmation prompt.
    pub fn aborted(current_version: String) -> Self {
        Self {
            status: "aborted",
            current_version,
            current_artifact: None,
            update_available: None,
            target_version: None,
            target_artifact: None,
            target_verification_error: None,
            install_method: None,
            upgrade_command: None,
            manual_reason: None,
            daemon: None,
            daemon_reason: None,
            daemon_pid: None,
            daemon_version: None,
            daemon_instance_id: None,
            daemon_listen: None,
        }
    }

    /// The updater ran but the binary was already current.
    pub fn unchanged(current_version: String) -> Self {
        Self {
            status: "unchanged",
            current_version,
            current_artifact: None,
            update_available: None,
            target_version: None,
            target_artifact: None,
            target_verification_error: None,
            install_method: None,
            upgrade_command: None,
            manual_reason: None,
            daemon: None,
            daemon_reason: None,
            daemon_pid: None,
            daemon_version: None,
            daemon_instance_id: None,
            daemon_listen: None,
        }
    }

    /// The binary was swapped to `new_version`. `daemon` records whether a
    /// running daemon was restarted onto it or still needs a manual restart.
    pub fn updated(
        current_version: String,
        new_version: String,
        daemon: Option<&'static str>,
    ) -> Self {
        Self {
            status: "updated",
            current_version,
            current_artifact: None,
            update_available: None,
            target_version: Some(new_version),
            target_artifact: None,
            target_verification_error: None,
            install_method: None,
            upgrade_command: None,
            manual_reason: None,
            daemon,
            daemon_reason: None,
            daemon_pid: None,
            daemon_version: None,
            daemon_instance_id: None,
            daemon_listen: None,
        }
    }
}

impl CliReport for UpdateReport {
    fn exit_code(&self) -> i32 {
        i32::from(self.daemon == Some("deferred"))
    }

    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        match self.status {
            "delegated" => {
                if let Some(reason) = &self.manual_reason {
                    h.line(reason)?;
                    h.line("Manual update command:")?;
                } else {
                    let how = match self.install_method {
                        Some("homebrew") => "Homebrew",
                        Some("cargo") => "Cargo",
                        Some("npm") => "npm",
                        _ => "your package manager",
                    };
                    h.line(&format!(
                        "bitrouter looks installed via {how}. Update with:"
                    ))?;
                }
                if let Some(cmd) = &self.upgrade_command {
                    h.line(&format!("    {cmd}"))?;
                }
                Ok(())
            }
            "checked" => {
                if self.update_available == Some(true) {
                    match &self.target_version {
                        Some(v) => h.line(&format!(
                            "update available: {} -> {v}",
                            self.current_version
                        )),
                        None => h.line("update available (target version unknown)"),
                    }
                } else {
                    h.line(&format!("up to date ({})", self.current_version))
                }
            }
            "aborted" => h.line("aborted"),
            "unchanged" => h.line(&format!("already up to date ({})", self.current_version)),
            "updated" => {
                let new = self.target_version.as_deref().unwrap_or("(unknown)");
                h.line(&format!("✓ updated {} -> {new}", self.current_version))?;
                if let Some(error) = &self.target_verification_error {
                    h.note(&format!("Installed artifact verification failed: {error}"))?;
                }
                match self.daemon {
                    Some("restarted") => h.note(&format!("Restarted the running daemon on {new}.")),
                    Some("compatible") => h.note(&format!("The running daemon is serving {new}.")),
                    Some("restart_needed") => h.note(&format!(
                        "A daemon is running the old binary. Run `{} restart` to serve {new}.",
                        crate::commands::invocation::name()
                    )),
                    Some("deferred") => h.note(&format!(
                        "Daemon handoff deferred: {}",
                        self.daemon_reason
                            .as_deref()
                            .unwrap_or("reason unavailable")
                    )),
                    _ => Ok(()),
                }?;
                if let Some(pid) = self.daemon_pid {
                    h.field("daemon pid", pid)?;
                }
                if let Some(version) = &self.daemon_version {
                    h.field("daemon version", version)?;
                }
                if let Some(instance) = &self.daemon_instance_id {
                    h.field("daemon instance", instance)?;
                }
                if let Some(listen) = &self.daemon_listen {
                    h.field("daemon listen", listen)?;
                }
                Ok(())
            }
            other => h.line(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};

    fn json(r: &dyn CliReport) -> serde_json::Value {
        serde_json::from_slice(&Output::new(Format::Json).render_to_vec(r)).unwrap()
    }

    fn human(r: &dyn CliReport) -> String {
        String::from_utf8(Output::new(Format::Human).render_to_vec(r)).unwrap()
    }

    #[test]
    fn delegated_carries_method_and_command() {
        let r = UpdateReport::delegated(
            "1.0.0-alpha.19".into(),
            "homebrew",
            "brew upgrade bitrouter".into(),
        );
        assert_eq!(
            json(&r),
            serde_json::json!({
                "status": "delegated",
                "current_version": "1.0.0-alpha.19",
                "install_method": "homebrew",
                "upgrade_command": "brew upgrade bitrouter"
            })
        );
        let h = human(&r);
        assert!(h.contains("looks installed via Homebrew"), "{h:?}");
        assert!(h.contains("    brew upgrade bitrouter"), "{h:?}");
    }

    #[test]
    fn manual_profile_result_preserves_identity_without_claiming_an_update() -> anyhow::Result<()> {
        let mut artifact = crate::artifact::current();
        artifact.profile = bitrouter_ai::artifact::DistributionProfile::Custom;
        artifact.features = vec!["core".into()];
        let mut report = UpdateReport::delegated(
            artifact.version.clone(),
            "homebrew",
            "cargo install bitrouter --force --no-default-features --features core".into(),
        )
        .with_current_artifact(artifact.clone());
        report.manual_reason = Some(
            "No matching release installer/receipt is available for the custom profile.".into(),
        );
        let value = json(&report);
        assert_eq!(value["status"], "delegated");
        assert_eq!(value["install_method"], "homebrew");
        assert_eq!(value["current_artifact"], serde_json::to_value(artifact)?);
        assert_eq!(
            value["manual_reason"].as_str(),
            report.manual_reason.as_deref()
        );
        for absent in [
            "update_available",
            "target_version",
            "target_artifact",
            "daemon",
        ] {
            assert!(value.get(absent).is_none());
        }
        let rendered = human(&report);
        assert!(rendered.contains("custom profile"));
        assert!(rendered.contains("--no-default-features --features core"));
        assert!(!rendered.contains("looks installed via Homebrew"));
        assert_eq!(report.exit_code(), 0);
        Ok(())
    }

    #[test]
    fn verified_target_identity_is_distinct_from_invoking_identity() -> anyhow::Result<()> {
        let current = crate::artifact::current();
        let mut target = current.clone();
        target.version = "9.9.9".into();
        target.source_revision = Some("verified-new-revision".into());
        target.handoff_build_id = "verified-new-build".into();
        let mut report =
            UpdateReport::updated(current.version.clone(), target.version.clone(), None)
                .with_current_artifact(current.clone());
        assert!(report.target_artifact.is_none());
        assert!(json(&report).get("target_artifact").is_none());
        report.target_artifact = Some(target.clone());
        assert_eq!(
            json(&report)["current_artifact"],
            serde_json::to_value(current)?
        );
        assert_eq!(
            json(&report)["target_artifact"],
            serde_json::to_value(target)?
        );
        Ok(())
    }

    #[test]
    fn checked_available_names_target_version() {
        let r = UpdateReport::checked("1.0.0-alpha.19".into(), true, Some("1.0.0-alpha.20".into()));
        assert_eq!(
            json(&r),
            serde_json::json!({
                "status": "checked",
                "current_version": "1.0.0-alpha.19",
                "update_available": true,
                "target_version": "1.0.0-alpha.20"
            })
        );
        assert_eq!(
            human(&r),
            "update available: 1.0.0-alpha.19 -> 1.0.0-alpha.20\n"
        );
    }

    #[test]
    fn checked_up_to_date_omits_target() {
        let r = UpdateReport::checked("1.0.0-alpha.19".into(), false, None);
        assert_eq!(
            json(&r),
            serde_json::json!({
                "status": "checked",
                "current_version": "1.0.0-alpha.19",
                "update_available": false
            })
        );
        assert_eq!(human(&r), "up to date (1.0.0-alpha.19)\n");
    }

    #[test]
    fn checked_available_but_target_unknown() {
        // `is_update_needed()` is true but `query_new_version()` came back empty.
        let r = UpdateReport::checked("1.0.0-alpha.19".into(), true, None);
        assert_eq!(
            json(&r),
            serde_json::json!({
                "status": "checked",
                "current_version": "1.0.0-alpha.19",
                "update_available": true
            })
        );
        assert_eq!(human(&r), "update available (target version unknown)\n");
    }

    #[test]
    fn updated_without_running_daemon_omits_daemon_key() {
        let r = UpdateReport::updated("1.0.0-alpha.19".into(), "1.0.0-alpha.20".into(), None);
        assert_eq!(
            json(&r),
            serde_json::json!({
                "status": "updated",
                "current_version": "1.0.0-alpha.19",
                "target_version": "1.0.0-alpha.20"
            })
        );
        // No running daemon -> just the result line, no follow-up note.
        assert_eq!(human(&r), "✓ updated 1.0.0-alpha.19 -> 1.0.0-alpha.20\n");
    }

    #[test]
    fn installed_legacy_target_reports_verification_error_without_a_daemon() {
        let mut report =
            UpdateReport::updated("1.0.0-alpha.19".into(), "1.0.0-alpha.18".into(), None);
        report.target_verification_error =
            Some("legacy release does not support build-info".into());
        assert_eq!(
            json(&report),
            serde_json::json!({
                "status": "updated",
                "current_version": "1.0.0-alpha.19",
                "target_version": "1.0.0-alpha.18",
                "target_verification_error": "legacy release does not support build-info"
            })
        );
        let rendered = human(&report);
        assert!(rendered.contains("✓ updated 1.0.0-alpha.19 -> 1.0.0-alpha.18"));
        assert!(rendered.contains(
            "Installed artifact verification failed: legacy release does not support build-info"
        ));
        assert!(!rendered.contains("Daemon handoff"));
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn updated_with_restart_hint_renders_note() {
        let r = UpdateReport::updated(
            "1.0.0-alpha.19".into(),
            "1.0.0-alpha.20".into(),
            Some("restart_needed"),
        );
        assert_eq!(
            json(&r),
            serde_json::json!({
                "status": "updated",
                "current_version": "1.0.0-alpha.19",
                "target_version": "1.0.0-alpha.20",
                "daemon": "restart_needed"
            })
        );
        let h = human(&r);
        assert!(
            h.contains("✓ updated 1.0.0-alpha.19 -> 1.0.0-alpha.20"),
            "{h:?}"
        );
        assert!(h.contains("Run `bro restart`"), "{h:?}");
    }

    #[test]
    fn updated_restarted_omits_hint() {
        let r = UpdateReport::updated(
            "1.0.0-alpha.19".into(),
            "1.0.0-alpha.20".into(),
            Some("restarted"),
        );
        let h = human(&r);
        assert!(
            h.contains("Restarted the running daemon on 1.0.0-alpha.20"),
            "{h:?}"
        );
    }

    #[test]
    fn already_compatible_daemon_does_not_claim_restart() {
        let report = UpdateReport::updated(
            "1.0.0-alpha.31".into(),
            "1.0.0-alpha.32".into(),
            Some("compatible"),
        );
        assert_eq!(report.exit_code(), 0);
        assert!(human(&report).contains("The running daemon is serving 1.0.0-alpha.32"));
    }

    #[test]
    fn updated_binary_with_deferred_daemon_is_not_a_success() {
        let mut report = UpdateReport::updated(
            "1.0.0-alpha.31".into(),
            "1.0.0-alpha.32".into(),
            Some("deferred"),
        );
        report.daemon_reason = Some("one supervised run is still active".into());
        assert_eq!(report.exit_code(), 1);
        assert_eq!(json(&report)["daemon"], "deferred");
        assert!(human(&report).contains("one supervised run is still active"));
    }

    #[test]
    fn unchanged_and_aborted_are_one_liners() {
        assert_eq!(
            human(&UpdateReport::unchanged("1.0.0-alpha.19".into())),
            "already up to date (1.0.0-alpha.19)\n"
        );
        assert_eq!(
            human(&UpdateReport::aborted("1.0.0-alpha.19".into())),
            "aborted\n"
        );
        assert_eq!(
            json(&UpdateReport::aborted("1.0.0-alpha.19".into())),
            serde_json::json!({"status": "aborted", "current_version": "1.0.0-alpha.19"})
        );
    }
}
