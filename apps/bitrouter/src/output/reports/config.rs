//! Reports for `init` and `config validate`.

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;

/// Result of `bitrouter init`.
#[derive(Serialize)]
pub struct InitReport {
    pub action: &'static str,
    pub path: String,
    pub skip_auth: bool,
}

impl CliReport for InitReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!("wrote starter config to {}", self.path))?;
        h.note("skip_auth is on — credential-less local requests are admitted")
    }
}

/// One unset `${VAR}` substituted with a placeholder during validation.
#[derive(Serialize)]
pub struct UnsetVar {
    pub unset_env: String,
}

/// Result of `bitrouter config validate`. `valid: false` carries `errors` and
/// exits non-zero (CI-safe); `valid: true` carries the catalog counts and any
/// unset-var `warnings`.
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
            errors: Vec::new(),
        }
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
