//! Reports for `config validate`. (`bitrouter init` now emits the onboarding
//! result envelope via `crate::onboarding` rather than a dedicated report.)

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;

/// One unset `${VAR}` substituted with a placeholder during validation.
#[derive(Serialize)]
pub struct UnsetVar {
    pub unset_env: String,
}

/// Result of `bitrouter config validate`. `valid: false` carries `errors` and
/// exits non-zero (CI-safe); `valid: true` carries the catalog counts, any
/// unset-var `warnings`, and any `unknown_plugins`.
///
/// `ignored_config` is a separate field rather than another `warnings` entry
/// because the two are different shapes and a consumer already parses
/// `warnings[].unset_env`. Neither fails the validation: config the binary
/// ignores is a misconfiguration, not a malformed config, and `valid` is what
/// CI gates on.
///
/// It carries the same lines the daemon logs at startup, minus the environment
/// ones — this command validates a file, which may not belong to the machine
/// running it.
#[derive(Serialize)]
pub struct ValidateReport {
    pub valid: bool,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub providers: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub presets: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variants: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<UnsetVar>,
    /// Configuration present in the file that the binary will not act on.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ignored_config: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

impl ValidateReport {
    pub fn valid(
        path: String,
        providers: usize,
        models: usize,
        presets: usize,
        variants: usize,
        warnings: Vec<UnsetVar>,
    ) -> Self {
        Self {
            valid: true,
            path,
            providers: Some(providers),
            models: Some(models),
            presets: Some(presets),
            variants: Some(variants),
            warnings,
            ignored_config: Vec::new(),
            errors: Vec::new(),
        }
    }

    /// Attach the configuration the binary will ignore. Separate from
    /// [`Self::valid`] so its argument list does not keep growing.
    pub fn with_ignored_config(mut self, ignored_config: Vec<String>) -> Self {
        self.ignored_config = ignored_config;
        self
    }

    pub fn invalid(path: String, error: String) -> Self {
        Self {
            valid: false,
            path,
            providers: None,
            models: None,
            presets: None,
            variants: None,
            warnings: Vec::new(),
            ignored_config: Vec::new(),
            errors: vec![error],
        }
    }
}

impl CliReport for ValidateReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if self.valid {
            h.line(&format!("✓ {} is valid", self.path))?;
            h.line(&format!(
                "  providers: {}  models: {}  presets: {}  variants: {}",
                self.providers.unwrap_or(0),
                self.models.unwrap_or(0),
                self.presets.unwrap_or(0),
                self.variants.unwrap_or(0),
            ))?;
            if !self.warnings.is_empty() {
                h.blank()?;
                h.line(&format!(
                    "  note: {} unset environment variable(s) substituted with a placeholder \
                     for validation (re-validate at runtime):",
                    self.warnings.len()
                ))?;
                for w in &self.warnings {
                    h.line(&format!("    - ${{{}}}", w.unset_env))?;
                }
            }
            if !self.ignored_config.is_empty() {
                h.blank()?;
                h.line(&format!(
                    "  note: {} setting(s) this binary does not act on — ignored, \
                     which is silent at runtime:",
                    self.ignored_config.len()
                ))?;
                for line in &self.ignored_config {
                    h.line(&format!("    - {line}"))?;
                }
            }
            Ok(())
        } else {
            h.line(&format!("✗ {} is invalid", self.path))?;
            for e in &self.errors {
                h.line(&format!("  {e}"))?;
            }
            Ok(())
        }
    }

    fn exit_code(&self) -> i32 {
        if self.valid { 0 } else { 1 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::CliReport;

    #[test]
    fn ignored_config_is_reported_without_failing_validation() {
        let report = ValidateReport::valid("p".into(), 1, 0, 0, 0, vec![])
            .with_ignored_config(vec!["plugins.bitrouter-guardrail is ignored".into()]);
        // A misconfiguration, not a malformed config: CI gates on `valid`, and
        // an ignored block must not turn a green pipeline red.
        assert_eq!(report.exit_code(), 0);
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(
            v["ignored_config"][0],
            "plugins.bitrouter-guardrail is ignored"
        );
        // and it is omitted entirely when there is nothing to say.
        let clean = ValidateReport::valid("p".into(), 1, 0, 0, 0, vec![]);
        assert!(
            serde_json::to_value(&clean)
                .unwrap()
                .get("ignored_config")
                .is_none()
        );
    }

    #[test]
    fn validate_exit_code_and_shape() {
        let ok = ValidateReport::valid("p".into(), 1, 2, 0, 0, vec![]);
        assert_eq!(ok.exit_code(), 0);
        let bad = ValidateReport::invalid("p".into(), "boom".into());
        assert_eq!(bad.exit_code(), 1);
        let v = serde_json::to_value(&bad).unwrap();
        assert_eq!(v["valid"], false);
        assert_eq!(v["errors"][0], "boom");
        // valid report omits the empty errors array.
        assert!(serde_json::to_value(&ok).unwrap().get("errors").is_none());
    }
}
