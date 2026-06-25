//! Report for `bitrouter verify <model>` — the L1 TEE-attestation verdict.

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;

/// One attestation check. `status` is `pass` / `fail` / `skip` (a check that
/// did not run, e.g. an optional event-log anchor).
#[derive(Serialize)]
pub struct Check {
    pub name: String,
    pub status: &'static str,
}

/// Result of `bitrouter verify <model>`.
#[derive(Serialize)]
pub struct VerifyReport {
    pub model: String,
    pub trust_boundary: String,
    pub verified: bool,
    pub checks: Vec<Check>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub signers: Vec<String>,
}

impl CliReport for VerifyReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        let tb = if self.trust_boundary.is_empty() {
            "unreachable"
        } else {
            &self.trust_boundary
        };
        h.line(&format!("{}  (trust boundary: {tb})", self.model))?;
        for c in &self.checks {
            let mark = match c.status {
                "pass" => "✓",
                "fail" => "✗",
                _ => "-",
            };
            h.line(&format!("  {mark} {}", c.name))?;
        }
        if !self.signers.is_empty() {
            h.line(&format!(
                "  attested signer(s): {}",
                self.signers.join(", ")
            ))?;
        }
        h.blank()?;
        if self.verified {
            h.line("VERIFIED — genuine, policy-pinned TEE")
        } else {
            h.line("UNVERIFIED — not a confirmed legitimate TEE (see failing checks above)")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::{Format, Output};

    #[test]
    fn verify_json_and_glyphs() {
        let r = VerifyReport {
            model: "m".into(),
            trust_boundary: "tb".into(),
            verified: false,
            checks: vec![
                Check {
                    name: "a".into(),
                    status: "pass",
                },
                Check {
                    name: "b".into(),
                    status: "fail",
                },
                Check {
                    name: "c".into(),
                    status: "skip",
                },
            ],
            signers: vec!["0xabc".into()],
        };
        let v: serde_json::Value =
            serde_json::from_slice(&Output::new(Format::Json).render_to_vec(&r)).unwrap();
        assert_eq!(v["verified"], false);
        assert_eq!(v["checks"][1]["status"], "fail");
        assert_eq!(v["signers"][0], "0xabc");
        // verify keeps exit 0 even when UNVERIFIED (verdict lives in the JSON).
        assert_eq!(r.exit_code(), 0);

        let h = String::from_utf8(Output::new(Format::Human).render_to_vec(&r)).unwrap();
        assert!(h.contains("✓ a"), "{h:?}");
        assert!(h.contains("✗ b"), "{h:?}");
        assert!(h.contains("- c"), "{h:?}");
        assert!(h.contains("UNVERIFIED"), "{h:?}");
    }
}
