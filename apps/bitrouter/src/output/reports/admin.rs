//! Reports for key minting, policy scaffolding, and per-provider credential
//! login/logout.

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;

/// Result of `bitrouter key sign`. The plaintext `secret` is shown once — only
/// its SHA-256 hash is stored.
#[derive(Serialize)]
pub struct KeySignReport {
    pub id: String,
    pub user: String,
    pub secret: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    pub hash_stored: bool,
}

impl CliReport for KeySignReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "created virtual key {} for user '{}'",
            self.id, self.user
        ))?;
        h.blank()?;
        h.line(&format!("  {}", self.secret))?;
        h.blank()?;
        h.line("This secret is shown ONCE — only its SHA-256 hash is stored.")
    }
}

/// Result of `bitrouter policy create <id>`.
#[derive(Serialize)]
pub struct PolicyCreateReport {
    pub id: String,
    pub path: String,
    pub created: bool,
}

impl CliReport for PolicyCreateReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!("wrote starter policy to {}", self.path))?;
        h.line("  edit, then bind to a key with:")?;
        h.line(&format!(
            "    bitrouter key sign --user <id> --policy {}",
            self.id
        ))
    }
}

/// Result of `bitrouter providers login <provider>`.
#[derive(Serialize)]
pub struct ProviderLoginReport {
    pub provider: String,
    pub label: String,
    pub method: String,
    pub credential: &'static str,
    pub path: String,
}

impl CliReport for ProviderLoginReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "✓ saved {} credential for {} (label: {}) at {}",
            self.method, self.provider, self.label, self.path
        ))
    }
}

/// Result of `bitrouter providers logout <provider>`.
#[derive(Serialize)]
pub struct ProviderLogoutReport {
    pub provider: String,
    pub removed: usize,
}

impl CliReport for ProviderLogoutReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        if self.removed == 0 {
            h.line(&format!(
                "no stored credentials for {}; nothing to remove",
                self.provider
            ))
        } else {
            h.line(&format!(
                "✓ removed {} stored credential(s) for {}",
                self.removed, self.provider
            ))
        }
    }
}
