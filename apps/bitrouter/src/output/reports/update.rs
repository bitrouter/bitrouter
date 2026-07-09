//! Report for the `bitrouter update` self-updater.
//!
//! Every terminal outcome of `update` — a package-manager delegation, a
//! `--check` probe, a declined confirmation, a no-op, or a completed swap —
//! is one [`UpdateReport`]. Interactive prompts and progress are diagnostics
//! and go to stderr; this is the single result value on stdout.

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;

/// Result of `bitrouter update`. `status` is the discriminant; the remaining
/// fields are populated only for the outcomes they belong to.
#[derive(Debug, Serialize)]
pub struct UpdateReport {
    /// `delegated` | `checked` | `aborted` | `unchanged` | `updated`.
    pub status: &'static str,
    /// The version of the currently-running binary.
    pub current_version: String,
    /// `checked`: whether a newer release exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_available: Option<bool>,
    /// The version this outcome refers to: the available release (`checked`)
    /// or the freshly-installed one (`updated`). Absent when unknown / N/A.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_version: Option<String>,
    /// `delegated`: how the binary was installed (`homebrew` | `cargo` |
    /// `unknown`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_method: Option<&'static str>,
    /// `delegated`: the exact command to run to upgrade out-of-band.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade_command: Option<String>,
    /// `updated`: the running daemon's fate — `restarted` (we restarted it) or
    /// `restart_needed` (still on the old binary). Absent when no daemon runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon: Option<&'static str>,
}

impl UpdateReport {
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
            update_available: None,
            target_version: None,
            install_method: Some(install_method),
            upgrade_command: Some(upgrade_command),
            daemon: None,
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
            update_available: Some(available),
            target_version,
            install_method: None,
            upgrade_command: None,
            daemon: None,
        }
    }

    /// The user declined the confirmation prompt.
    pub fn aborted(current_version: String) -> Self {
        Self {
            status: "aborted",
            current_version,
            update_available: None,
            target_version: None,
            install_method: None,
            upgrade_command: None,
            daemon: None,
        }
    }

    /// The updater ran but the binary was already current.
    pub fn unchanged(current_version: String) -> Self {
        Self {
            status: "unchanged",
            current_version,
            update_available: None,
            target_version: None,
            install_method: None,
            upgrade_command: None,
            daemon: None,
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
            update_available: None,
            target_version: Some(new_version),
            install_method: None,
            upgrade_command: None,
            daemon,
        }
    }
}

impl CliReport for UpdateReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        match self.status {
            "delegated" => {
                let how = match self.install_method {
                    Some("homebrew") => "Homebrew",
                    Some("cargo") => "Cargo",
                    _ => "your package manager",
                };
                h.line(&format!(
                    "bitrouter looks installed via {how}. Update with:"
                ))?;
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
                match self.daemon {
                    Some("restarted") => {
                        h.note(&format!("Restarted the running daemon on {new}."))
                    }
                    Some("restart_needed") => h.note(&format!(
                        "A daemon is running the old binary. Run `bitrouter restart` to serve {new}."
                    )),
                    _ => Ok(()),
                }
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
        assert!(h.contains("Run `bitrouter restart`"), "{h:?}");
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
