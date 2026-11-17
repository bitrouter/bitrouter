//! CLI never shares the broad developer credential store.
use super::{Controller, KeyStorage, Status};
use crate::output::{CliReport, Format, Output, human::Human};
use anyhow::{Context, Result, bail};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum AgentAction {
    /// Pair a new connection, or resume a named pending profile.
    Connect {
        /// Local profile ID; omitted creates an independent profile.
        #[arg(long)]
        profile: Option<String>,
        #[arg(long, default_value = "BitRouter Agent")]
        name: String,
        /// Canonical Console issuer. Never inferred from Cloud API metadata.
        #[arg(long, default_value = "https://cloud.bitrouter.ai")]
        issuer: String,
        /// Explicit Cloud audience expected from the issuer.
        #[arg(long, default_value = "https://api.bitrouter.ai")]
        resource: String,
        /// File storage is an explicit fallback; no silent downgrade.
        #[arg(long, value_enum, default_value = "keychain")]
        key_storage: KeyStorage,
        /// Ask the browser to approve a same-key successor to this profile.
        #[arg(long, requires = "profile")]
        renew: bool,
    },
    /// Read current authority; resume confirmation and rotate near-expiry tokens.
    Status {
        #[arg(long)]
        profile: String,
    },
    /// Revoke only this profile's credentials, then erase its local secrets.
    Disconnect {
        #[arg(long)]
        profile: String,
    },
    /// Local stdio MCP auth bridge; never registers a remote origin server.
    Mcp {
        #[arg(long, default_value = "https://cloud.bitrouter.ai")]
        issuer: String,
        #[arg(long, default_value = "https://api.bitrouter.ai")]
        resource: String,
        #[arg(long, value_enum, default_value = "keychain")]
        key_storage: KeyStorage,
    },
}
impl CliReport for Status {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.field("profile", &self.profile)?;
        h.field("status", &self.status)?;
        if let Some(agent) = &self.agent_id {
            h.field("agent", agent)?;
        }
        if let Some(connection) = &self.connection_id {
            h.field("connection", connection)?;
        }
        if let Some(uri) = &self.verification_uri {
            h.field("approve", uri)?;
        }
        if let Some(code) = &self.user_code {
            h.field("code", code)?;
        }
        Ok(())
    }
}
pub async fn run(action: AgentAction, format: Format) -> Result<()> {
    if let AgentAction::Mcp {
        issuer,
        resource,
        key_storage,
    } = action
    {
        #[cfg(feature = "mcp")]
        return super::mcp::serve(issuer, resource, key_storage).await;
        #[cfg(not(feature = "mcp"))]
        {
            let _ = (issuer, resource, key_storage);
            bail!("Agent MCP requires the mcp feature");
        }
    }
    let controller = Controller::default()?;
    let status = match action {
        AgentAction::Connect {
            profile,
            name,
            issuer,
            resource,
            key_storage,
            renew,
        } => {
            let profile = profile.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            if profile.is_empty()
                || profile.len() > 100
                || !profile
                    .bytes()
                    .all(|v| v.is_ascii_alphanumeric() || matches!(v, b'_' | b'-'))
            {
                bail!("invalid Agent profile ID");
            }
            eprintln!("Local Agent profile: {profile}");
            let mut status = controller
                .begin(&profile, &name, &issuer, &resource, key_storage, renew)
                .await
                .with_context(|| {
                    format!("Agent profile {profile}; resume this profile using its original name")
                })?;
            if let (Some(uri), Some(code)) = (&status.verification_uri, &status.user_code) {
                eprintln!(
                    "Approve this Agent in your browser:\n  {uri}\nPairing code: {code}\nLocal profile: {profile}"
                );
            }
            loop {
                if !matches!(
                    status.status.as_str(),
                    "awaiting_approval" | "awaiting_confirmation" | "saved_connection"
                ) {
                    break;
                }
                tokio::select! {
                    _=tokio::signal::ctrl_c()=>{eprintln!("Stopped waiting. Resume with bro cloud agent status --profile {profile}");break;},
                    _=tokio::time::sleep(std::time::Duration::from_secs(5))=>{}
                }
                status = controller.status(&profile).await?;
            }
            if matches!(
                status.status.as_str(),
                "delivery_uncertain" | "refresh_uncertain" | "failed" | "denied" | "expired"
            ) {
                bail!(
                    "Agent connection {} ({profile}); inspect status and start a fresh pairing or renew its predecessor",
                    status.status
                );
            }
            status
        }
        AgentAction::Status { profile } => controller.status(&profile).await?,
        AgentAction::Disconnect { profile } => controller.disconnect(&profile).await?,
        AgentAction::Mcp { .. } => bail!("Agent MCP dispatch unavailable"),
    };
    Output::new(format).emit(&status)?;
    Ok(())
}
