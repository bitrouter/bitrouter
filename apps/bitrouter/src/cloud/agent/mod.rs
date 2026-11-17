//! First-party browser-approved Agent connections, separate from account login.
pub mod cli;
mod crypto;
#[cfg(feature = "mcp")]
mod mcp;
mod store;
#[cfg(test)]
mod tests;
mod wire;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;
use store::Store;

#[derive(Clone, Copy, Default, clap::ValueEnum)]
pub enum KeyStorage {
    #[default]
    Keychain,
    File,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum KeyRef {
    Keychain { account: String },
    File { pkcs8: String },
}
#[derive(Serialize, Deserialize)]
struct Profile {
    format: u8,
    id: String,
    issuer: String,
    resource: String,
    key: Option<KeyRef>,
    current: Option<Session>,
    pending: Option<Attempt>,
    refresh_uncertain: bool,
    disconnecting: bool,
    disconnected: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Session {
    client_id: String,
    connection_id: String,
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    agent_id: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Attempt {
    operation_id: String,
    name: String,
    renewal_connection_id: Option<String>,
    client_id: Option<String>,
    device: Option<Device>,
    phase: String,
    session: Option<Session>,
    confirmation_deadline: Option<i64>,
}
#[derive(Serialize, Deserialize)]
struct Device {
    code: String,
    user_code: String,
    verification_uri: String,
    expires_at: i64,
    interval: i64,
    next_poll_at: i64,
}

/// Safe output shared by CLI and local MCP; never a server JSON passthrough.
#[derive(Default, Serialize)]
pub struct Status {
    pub profile: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daily_limit_microusd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub scopes: Vec<String>,
    pub spending_enabled: bool,
}
fn now() -> i64 {
    Utc::now().timestamp()
}
fn id(value: &Value, field: &str, prefix: &str) -> Result<String> {
    let value = value
        .get(field)
        .and_then(Value::as_str)
        .context("Agent response lacks an identity binding")?;
    if !value.starts_with(prefix)
        || value.len() > 256
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        bail!("invalid Agent identity binding");
    }
    Ok(value.into())
}
fn text(value: &Value, field: &str, max: usize) -> Result<String> {
    let value = value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty() && value.len() <= max)
        .context("incomplete Agent protocol response")?;
    Ok(value.into())
}
fn timestamp(value: &Value, field: &str) -> Result<i64> {
    Ok(DateTime::parse_from_rfc3339(&text(value, field, 64)?)?.timestamp())
}
fn iso(value: i64) -> Result<String> {
    Ok(DateTime::from_timestamp(value, 0)
        .context("invalid Agent timestamp")?
        .to_rfc3339())
}
fn pending_status(profile: &Profile) -> Result<Status> {
    let mut status = Status {
        profile: profile.id.clone(),
        status: if profile.disconnected {
            "disconnected"
        } else if profile.disconnecting {
            "revocation_pending"
        } else if profile.refresh_uncertain {
            "refresh_uncertain"
        } else {
            "saved_connection"
        }
        .into(),
        ..Status::default()
    };
    if let Some(current) = &profile.current {
        status.agent_id = current.agent_id.clone();
        status.connection_id = Some(current.connection_id.clone());
    }
    if let Some(pending) = &profile.pending
        && !profile.disconnecting
        && !profile.disconnected
    {
        status.status = pending.phase.clone();
        status.name = Some(pending.name.clone());
        if let Some(device) = &pending.device {
            status.user_code = Some(device.user_code.clone());
            status.verification_uri = Some(device.verification_uri.clone());
            status.expires_at = Some(iso(device.expires_at)?);
        }
        if let Some(session) = &pending.session {
            status.connection_id = Some(session.connection_id.clone());
        }
    }
    Ok(status)
}
fn tokens(value: Value, client_id: &str) -> Result<Session> {
    if value
        .get("token_type")
        .and_then(Value::as_str)
        .is_none_or(|value| !value.eq_ignore_ascii_case("DPoP"))
    {
        bail!("Agent token is not proof-bound; refusing bearer fallback");
    }
    let expires = value
        .get("expires_in")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0 && *v <= 300)
        .context("invalid Agent token lifetime")?;
    let scope = text(&value, "scope", 256)?;
    if !scope
        .split_whitespace()
        .all(|s| wire::SCOPES.split_whitespace().any(|allowed| allowed == s))
        || !scope.split_whitespace().any(|s| s == "agent:self:read")
    {
        bail!("Agent token scopes mismatch");
    }
    Ok(Session {
        client_id: client_id.into(),
        connection_id: id(&value, "connection_id", "connection_")?,
        access_token: text(&value, "access_token", 16384)?,
        refresh_token: text(&value, "refresh_token", 8192)?,
        expires_at: now() + expires,
        agent_id: None,
    })
}

struct Controller {
    root: PathBuf,
    authority: Option<(String, String)>,
}
impl Controller {
    fn default() -> Result<Self> {
        Ok(Self {
            root: store::default_root()?,
            authority: None,
        })
    }
    fn check_authority(&self, profile: &Profile) -> Result<()> {
        if let Some((issuer, resource)) = &self.authority
            && (&profile.issuer != issuer || &profile.resource != resource)
        {
            bail!("saved Agent authority differs from this MCP bridge");
        }
        Ok(())
    }
    async fn begin(
        &self,
        profile_id: &str,
        name: &str,
        issuer: &str,
        resource: &str,
        storage: KeyStorage,
        renew: bool,
    ) -> Result<Status> {
        if name.trim().is_empty()
            || name.chars().count() > 100
            || name.chars().any(char::is_control)
        {
            bail!("Agent name must contain 1–100 printable characters");
        }
        let issuer = wire::origin(issuer)?;
        let resource = wire::resource(resource)?;
        let metadata = wire::discover(&issuer, &resource).await?;
        let store = Store::lock(&self.root, profile_id)?;
        let mut profile = match store.load()? {
            Some(profile) => profile,
            None => {
                if renew {
                    bail!("renewal requires an existing Agent profile");
                }
                let key = store::create_key(storage)?;
                let profile = Profile {
                    format: 1,
                    id: profile_id.into(),
                    issuer: issuer.clone(),
                    resource: resource.clone(),
                    key: Some(key),
                    current: None,
                    pending: None,
                    refresh_uncertain: false,
                    disconnecting: false,
                    disconnected: false,
                };
                store.save(&profile)?;
                profile
            }
        };
        if profile.issuer != issuer || profile.resource != resource {
            bail!("saved Agent authority differs; use a new profile");
        }
        if profile.disconnected || profile.disconnecting {
            bail!("Agent profile is disconnected or awaiting revocation; use a new profile");
        }
        let secret = store::read_key(profile.key.as_ref().context("Agent signing key missing")?)?;
        let mut transport = wire::Transport::new(secret.clone())?;
        // A saved candidate may already be the current server authority, even
        // when an older client marked it expired. Reconcile it before renewal.
        if profile
            .pending
            .as_ref()
            .is_some_and(|attempt| attempt.session.is_some())
        {
            let recovered = progress(&store, &mut profile, &metadata, &mut transport).await?;
            if !(renew
                && profile.current.is_some()
                && matches!(recovered.status.as_str(), "expired" | "failed"))
            {
                return Ok(recovered);
            }
        }
        if profile.pending.as_ref().is_some_and(|a| {
            matches!(
                a.phase.as_str(),
                "delivery_uncertain" | "expired" | "denied" | "failed"
            )
        }) && renew
            && profile.current.is_some()
        {
            if let Some(client) = profile
                .pending
                .as_ref()
                .and_then(|attempt| attempt.client_id.as_deref())
            {
                cancel_client(&metadata, &mut transport, client).await?;
            }
            profile.pending = None;
            store.save(&profile)?;
        }
        if profile.pending.is_none() {
            if profile.current.is_some() && !renew {
                return pending_status(&profile);
            }
            if profile.current.is_none() && renew {
                bail!("renewal requires a confirmed predecessor");
            }
            profile.pending = Some(Attempt {
                operation_id: uuid::Uuid::new_v4().to_string(),
                name: name.trim().into(),
                renewal_connection_id: profile
                    .current
                    .as_ref()
                    .map(|current| current.connection_id.clone()),
                client_id: None,
                device: None,
                phase: "registering".into(),
                session: None,
                confirmation_deadline: None,
            });
            store.save(&profile)?;
        }
        let pending = profile
            .pending
            .as_ref()
            .context("missing pairing operation")?;
        if pending.name != name.trim() {
            bail!("pending operation has a different name; resume with the original name");
        }
        if pending.client_id.is_none() {
            let mut body = json!({"operation_id":pending.operation_id,"name":pending.name,"public_jwk":crypto::public_jwk(&secret)?});
            if let Some(predecessor) = &pending.renewal_connection_id {
                body["renewal_connection_id"] = predecessor.clone().into();
            }
            let result = transport
                .key_action(&metadata.registration_endpoint, &body)
                .await?
                .require_ok()?;
            if result.get("profile_version").and_then(Value::as_u64) != Some(1)
                || result.get("resource").and_then(Value::as_str) != Some(resource.as_str())
            {
                bail!("registered Agent profile mismatch");
            }
            profile
                .pending
                .as_mut()
                .context("missing pairing")?
                .client_id = Some(id(&result, "client_id", "bitrouter-agent-")?);
            store.save(&profile)?;
        }
        let pending = profile.pending.as_ref().context("missing pairing")?;
        if pending.device.is_none() {
            let result = transport
                .machine(
                    &metadata
                        .authorization_server_metadata
                        .device_authorization_endpoint,
                    pending
                        .client_id
                        .as_deref()
                        .context("missing client binding")?,
                    &[("scope", wire::SCOPES.into()), ("resource", resource)],
                )
                .await?
                .require_ok()?;
            let raw_code = text(&result, "user_code", 32)?;
            let code = raw_code.replace(['-', ' '], "");
            if code.len() != 8
                || !code
                    .bytes()
                    .all(|v| v.is_ascii_uppercase() || v.is_ascii_digit())
            {
                bail!("invalid Agent pairing code");
            }
            let uri = text(&result, "verification_uri", 1024)?;
            if uri != format!("{issuer}/oauth/agent") {
                bail!("Agent verification page is outside the trusted control surface");
            }
            let expires = result
                .get("expires_in")
                .and_then(Value::as_i64)
                .filter(|v| *v > 0 && *v <= 600)
                .context("invalid Agent pairing lifetime")?;
            let interval = result
                .get("interval")
                .and_then(Value::as_i64)
                .unwrap_or(5)
                .clamp(5, 60);
            let pending = profile.pending.as_mut().context("missing pairing")?;
            pending.device = Some(Device {
                code: text(&result, "device_code", 8192)?,
                user_code: raw_code,
                verification_uri: format!("{uri}?user_code={code}"),
                expires_at: now() + expires,
                interval,
                next_poll_at: now() + interval,
            });
            pending.phase = "awaiting_approval".into();
            store.save(&profile)?;
        }
        pending_status(&profile)
    }

    async fn status(&self, profile_id: &str) -> Result<Status> {
        let store = Store::lock(&self.root, profile_id)?;
        let mut profile = store.load()?.context("Agent profile not found")?;
        self.check_authority(&profile)?;
        if profile.disconnected || profile.disconnecting {
            return pending_status(&profile);
        }
        let metadata = wire::discover(&profile.issuer, &profile.resource).await?;
        let secret = store::read_key(profile.key.as_ref().context("Agent signing key missing")?)?;
        let mut transport = wire::Transport::new(secret)?;
        if profile.pending.is_some() {
            return progress(&store, &mut profile, &metadata, &mut transport).await;
        }
        read_current(&store, &mut profile, &metadata, &mut transport).await
    }

    async fn disconnect(&self, profile_id: &str) -> Result<Status> {
        let store = Store::lock(&self.root, profile_id)?;
        let mut profile = store.load()?.context("Agent profile not found")?;
        self.check_authority(&profile)?;
        if !profile.disconnected {
            let metadata = wire::discover(&profile.issuer, &profile.resource).await?;
            let secret =
                store::read_key(profile.key.as_ref().context("Agent signing key missing")?)?;
            let mut transport = wire::Transport::new(secret)?;
            profile.disconnecting = true;
            store.save(&profile)?;
            let clients: std::collections::BTreeSet<&str> = profile
                .current
                .iter()
                .map(|session| session.client_id.as_str())
                .chain(
                    profile
                        .pending
                        .iter()
                        .filter_map(|attempt| attempt.client_id.as_deref()),
                )
                .collect();
            for client in clients {
                cancel_client(&metadata, &mut transport, client).await?;
            }
            profile.current = None;
            profile.pending = None;
            profile.refresh_uncertain = false;
            profile.disconnected = true;
            profile.disconnecting = false;
            store.save(&profile)?;
        }
        if let Some(key) = &profile.key {
            store::remove_key(key)?;
            profile.key = None;
            store.save(&profile)?;
        }
        pending_status(&profile)
    }
}

async fn cancel_client(
    metadata: &wire::Discovery,
    transport: &mut wire::Transport,
    client: &str,
) -> Result<()> {
    if !client.starts_with("bitrouter-agent-")
        || client.len() > 144
        || !client
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-'))
    {
        bail!("invalid saved Agent client binding");
    }
    let url = metadata
        .disconnect_endpoint_template
        .replace("{client_id}", client);
    transport.key_action(&url, &json!({})).await?.require_ok()?;
    Ok(())
}

async fn read_current(
    store: &Store,
    profile: &mut Profile,
    metadata: &wire::Discovery,
    transport: &mut wire::Transport,
) -> Result<Status> {
    if profile.refresh_uncertain {
        return pending_status(profile);
    }
    if profile
        .current
        .as_ref()
        .is_some_and(|session| session.expires_at <= now() + 30)
    {
        // Commit the uncertainty marker BEFORE rotating. A restarted process
        // must not replay a refresh whose replacement may have been lost.
        profile.refresh_uncertain = true;
        store.save(profile)?;
        let current = profile.current.as_ref().context("Agent session missing")?;
        let result = transport
            .machine(
                &metadata.authorization_server_metadata.token_endpoint,
                &current.client_id,
                &[
                    ("grant_type", "refresh_token".into()),
                    ("refresh_token", current.refresh_token.clone()),
                    ("resource", profile.resource.clone()),
                ],
            )
            .await?
            .require_ok()?;
        let mut replacement = tokens(result, &current.client_id)?;
        if replacement.connection_id != current.connection_id {
            bail!("refresh changed the Agent connection");
        }
        replacement.agent_id = current.agent_id.clone();
        profile.current = Some(replacement);
        profile.refresh_uncertain = false;
        store.save(profile)?;
    }
    let current = profile
        .current
        .as_ref()
        .context("Agent session missing; resume connect")?;
    let result = transport
        .protected(&metadata.self_endpoint, "GET", &current.access_token)
        .await?
        .require_ok()?;
    safe_self(&profile.id, current, result)
}

async fn progress(
    store: &Store,
    profile: &mut Profile,
    metadata: &wire::Discovery,
    transport: &mut wire::Transport,
) -> Result<Status> {
    let pending = profile.pending.as_ref().context("missing pairing")?;
    if pending.phase == "polling" {
        profile.pending.as_mut().context("missing pairing")?.phase = "delivery_uncertain".into();
        store.save(profile)?;
        return pending_status(profile);
    }
    if pending.session.is_none()
        && matches!(
            pending.phase.as_str(),
            "delivery_uncertain" | "expired" | "denied" | "failed" | "registering"
        )
    {
        return pending_status(profile);
    }
    if pending.session.is_none() {
        let device = pending
            .device
            .as_ref()
            .context("missing device authorization; resume connect")?;
        if device.expires_at <= now() {
            profile.pending.as_mut().context("missing pairing")?.phase = "expired".into();
            store.save(profile)?;
            return pending_status(profile);
        }
        if device.next_poll_at > now() {
            return pending_status(profile);
        }
        let client = pending
            .client_id
            .clone()
            .context("missing client binding")?;
        let code = device.code.clone();
        profile.pending.as_mut().context("missing pairing")?.phase = "polling".into();
        store.save(profile)?;
        let reply = transport
            .machine(
                &metadata.authorization_server_metadata.token_endpoint,
                &client,
                &[
                    ("grant_type", wire::DEVICE.into()),
                    ("device_code", code),
                    ("resource", profile.resource.clone()),
                ],
            )
            .await?;
        if reply.status != 200 {
            let pending = profile.pending.as_mut().context("missing pairing")?;
            pending.phase = match reply.error() {
                "authorization_pending" | "slow_down" => "awaiting_approval",
                "access_denied" => "denied",
                "expired_token" => "expired",
                _ => "delivery_uncertain",
            }
            .into();
            if let Some(device) = pending.device.as_mut() {
                if reply.error() == "slow_down" {
                    device.interval = (device.interval + 5).min(60);
                }
                device.next_poll_at = now() + device.interval;
            }
            store.save(profile)?;
            if !matches!(
                reply.error(),
                "authorization_pending" | "slow_down" | "access_denied" | "expired_token"
            ) {
                bail!(
                    "Agent delivery outcome is uncertain; use a fresh pairing or renew the predecessor"
                );
            }
            return pending_status(profile);
        }
        #[cfg(test)]
        tests::barrier(profile, "before-token-save").await?;
        let deadline = timestamp(&reply.body, "confirmation_deadline")?;
        if deadline <= now() || deadline > now() + 125 {
            bail!("invalid Agent confirmation deadline");
        }
        let session = tokens(reply.body, &client)?;
        let pending = profile.pending.as_mut().context("missing pairing")?;
        pending.session = Some(session);
        pending.confirmation_deadline = Some(deadline);
        pending.phase = "awaiting_confirmation".into();
        // Only the persisted replacement may confirm. A failed save leaves the
        // pre-request `polling` marker on disk and cannot activate the Grant.
        store.save(profile)?;
        #[cfg(test)]
        tests::barrier(profile, "after-token-save").await?;
    }
    let pending = profile.pending.as_ref().context("missing pairing")?;
    let session = pending
        .session
        .as_ref()
        .context("missing pending session")?;
    // The server receipt, not a local timeout, resolves an unknown activation.
    // This key-bound lookup remains usable after the saved access token expires.
    let url = metadata
        .activation_endpoint_template
        .replace("{client_id}", &session.client_id);
    let state = transport.key_action(&url, &json!({})).await?.require_ok()?;
    if state.get("client_id").and_then(Value::as_str) != Some(session.client_id.as_str())
        || state.get("connection_id").and_then(Value::as_str)
            != Some(session.connection_id.as_str())
    {
        bail!("Agent activation lookup changed connection");
    }
    match state.get("status").and_then(Value::as_str) {
        Some("active") => {
            return finish_activation(
                store,
                profile,
                metadata,
                transport,
                state
                    .get("activation")
                    .cloned()
                    .context("missing activation receipt")?,
            )
            .await;
        }
        Some("expired" | "unavailable") => {
            profile.pending.as_mut().context("missing pairing")?.phase =
                if state["status"] == "expired" {
                    "expired"
                } else {
                    "failed"
                }
                .into();
            store.save(profile)?;
            return pending_status(profile);
        }
        Some("pending") => {}
        _ => bail!("invalid Agent activation state"),
    }
    profile.pending.as_mut().context("missing pairing")?.phase = "awaiting_confirmation".into();
    store.save(profile)?;
    let pending = profile.pending.as_ref().context("missing pairing")?;
    // Never refresh an unconfirmed candidate. Only a recovered active receipt
    // can promote it into the ordinary, journaled current-session refresh path.
    if pending
        .session
        .as_ref()
        .is_some_and(|session| session.expires_at <= now())
    {
        return pending_status(profile);
    }
    let session = pending
        .session
        .as_ref()
        .context("missing pending session")?;
    // Re-establish durability after a previous rename/directory-sync ambiguity
    // before sending an activation, including after a process restart.
    store.save(profile)?;
    let url = metadata
        .confirmation_endpoint_template
        .replace("{connection_id}", &session.connection_id);
    let reply = transport
        .protected(&url, "POST", &session.access_token)
        .await?;
    if matches!(reply.status, 409 | 410) {
        profile.pending.as_mut().context("missing pairing")?.phase = "failed".into();
        store.save(profile)?;
        return pending_status(profile);
    }
    let receipt = reply.require_ok()?;
    finish_activation(store, profile, metadata, transport, receipt).await
}

async fn finish_activation(
    store: &Store,
    profile: &mut Profile,
    metadata: &wire::Discovery,
    transport: &mut wire::Transport,
    receipt: Value,
) -> Result<Status> {
    let session = profile
        .pending
        .as_ref()
        .and_then(|attempt| attempt.session.as_ref())
        .context("missing saved candidate")?;
    if receipt.get("ok").and_then(Value::as_bool) != Some(true)
        || receipt.get("connectionId").and_then(Value::as_str)
            != Some(session.connection_id.as_str())
    {
        bail!("Agent activation receipt mismatch");
    }
    let agent_id = id(&receipt, "agentId", "agent_")?;
    #[cfg(test)]
    tests::barrier(profile, "after-confirm").await?;
    let mut session = session.clone();
    session.agent_id = Some(agent_id);
    profile.current = Some(session);
    profile.pending = None;
    profile.refresh_uncertain = false;
    store.save(profile)?;
    read_current(store, profile, metadata, transport).await
}
fn safe_self(profile: &str, session: &Session, value: Value) -> Result<Status> {
    let connection = id(&value, "connection_id", "connection_")?;
    let agent = id(&value, "agent_id", "agent_")?;
    if connection != session.connection_id || session.agent_id.as_ref() != Some(&agent) {
        bail!("Agent self response changed identity");
    }
    let scopes: Vec<String> = serde_json::from_value(
        value
            .get("scopes")
            .cloned()
            .context("missing Agent scopes")?,
    )?;
    if scopes.is_empty()
        || scopes
            .iter()
            .any(|scope| !["agent:self:read", "inference:invoke"].contains(&scope.as_str()))
    {
        bail!("Agent self response has unexpected scopes");
    }
    let cap = text(&value, "daily_limit_microusd", 19)?;
    if cap.parse::<i64>().ok().is_none_or(|v| v <= 0) {
        bail!("Agent self response lacks a finite limit");
    }
    Ok(Status {
        profile: profile.into(),
        status: "connected".into(),
        agent_id: Some(agent),
        connection_id: Some(connection),
        expires_at: Some(iso(timestamp(&value, "expires_at")?)?),
        daily_limit_microusd: Some(cap),
        namespace_id: Some(id(&value, "namespace_id", "")?),
        name: Some(
            text(&value, "name", 400)?
                .chars()
                .filter(|c| !c.is_control())
                .collect(),
        ),
        scopes,
        spending_enabled: value
            .get("spending_enabled")
            .and_then(Value::as_bool)
            .context("missing Agent spending status")?,
        ..Status::default()
    })
}
