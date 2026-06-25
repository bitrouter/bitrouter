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
