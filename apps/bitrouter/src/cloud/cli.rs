//! `bitrouter cloud …` — typed wrappers around every endpoint on
//! [`bitrouter_cloud_sdk::management::ManagementClient`].
//!
//! The subcommand tree is grouped by resource (`keys`, `usage`,
//! `billing`, `policy`, `budget`, `preset`, `byok`, `oauth-client`)
//! plus a top-level `whoami` that prints the local identity + the
//! configured cloud base URL. Every leaf accepts `--json` for raw
//! response output; the default is a compact human-readable form.
//!
//! Spec inputs that need a free-form JSON body (the generic
//! `policy create` / `policy update` and every `preset create` /
//! `preset update` clause) are read from a file path or `-` for stdin.
//!
//! All errors flow through one place ([`run`]); 403 responses whose
//! description matches the server's `missing required scope: <s>` shape
//! get a tailored re-login hint pointing the user at
//! `bitrouter cloud login --scope "<existing scopes> <missing>"`.

use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use clap::{Subcommand, ValueEnum};

use bitrouter_cloud_sdk::auth::commands::{LoginInputs, login, logout};
use bitrouter_cloud_sdk::auth::credentials::{CredentialsStore, default_credentials_path};
use bitrouter_cloud_sdk::management::types::{
    BudgetWindow as SdkBudgetWindow, ClientType as SdkClientType, PolicyKind as SdkPolicyKind,
};
use bitrouter_cloud_sdk::management::{
    Error as SdkError, ManagementClient, billing, budgets, byok, keys, namespaces, oauth_clients,
    policies, presets, usage,
};

/// `bitrouter cloud …`. All variants land in [`run`].
#[derive(Debug, Subcommand)]
pub enum CloudAction {
    /// Print the cloud identity stored on this machine alongside the
    /// `/v1/*` base URL the CLI will target.
    Whoami,
    /// Sign in to BitRouter Cloud from this terminal.
    ///
    /// Prints a verification URL — open it, approve, and this CLI stores an
    /// access token it refreshes automatically. This is the same credential
    /// the built-in `bitrouter` provider uses for inference, so
    /// `providers login bitrouter` is an alias for this command.
    Login {
        /// Authorization server URL. Defaults to <https://api.bitrouter.ai>;
        /// override only for a self-hosted deployment (env: BITROUTER_OAUTH_AS).
        #[arg(long = "oauth-as", value_name = "URL")]
        authorization_server: Option<String>,
        /// OAuth client id. Defaults to `bitrouter-cli`; override only for a
        /// self-hosted deployment (env: BITROUTER_OAUTH_CLIENT_ID).
        #[arg(long = "client-id", value_name = "ID")]
        client_id: Option<String>,
        /// Permissions to request, as a space-delimited list. Defaults to a
        /// broad "developer" set; pass a narrower or wider list to override
        /// (env: BITROUTER_OAUTH_SCOPE).
        #[arg(long, value_name = "SCOPE")]
        scope: Option<String>,
    },
    /// Sign out: revoke the stored token at the server (best-effort) and
    /// delete the local credentials file.
    Logout {
        /// Override the authorization server URL recorded in the
        /// credentials file for the revocation call.
        #[arg(long = "oauth-as", value_name = "URL")]
        authorization_server: Option<String>,
        /// Override the recorded OAuth client id for the revocation call.
        #[arg(long = "client-id", value_name = "ID")]
        client_id: Option<String>,
    },
    /// Inspect the namespaces you own and the one this CLI is bound to.
    Namespace {
        #[command(subcommand)]
        action: NamespaceAction,
    },
    /// Manage `brk_` API keys in your namespace.
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// Read aggregate spend / token counts for your account.
    Usage(UsageArgs),
    /// Page through recent inference requests.
    Requests(RequestsArgs),
    /// Credit balance and Stripe checkout.
    Billing {
        #[command(subcommand)]
        action: BillingAction,
    },
    /// Generic CRUD over the typed policy registry.
    Policy {
        #[command(subcommand)]
        action: PolicyAction,
    },
    /// Typed wrapper over budget-kind policies.
    Budget {
        #[command(subcommand)]
        action: BudgetAction,
    },
    /// Typed wrapper over preset-kind policies.
    Preset {
        #[command(subcommand)]
        action: PresetAction,
    },
    /// Bring-your-own-key provider keys.
    Byok {
        #[command(subcommand)]
        action: ByokAction,
    },
    /// Registered OAuth clients on your account.
    #[command(name = "oauth-client")]
    OauthClient {
        #[command(subcommand)]
        action: OauthClientAction,
    },
}

// ===== Namespace =====

#[derive(Debug, Subcommand)]
pub enum NamespaceAction {
    /// List the namespaces you own. The one this CLI is signed in to is
    /// marked `(active)`. Switching namespaces is a re-login:
    /// `bitrouter cloud login` and pick a different namespace in the
    /// browser.
    List(JsonFlag),
    /// Print the namespace this CLI's credential is bound to. Offline —
    /// reads the local credential, no network call.
    Current(JsonFlag),
}

// ===== Keys =====

#[derive(Debug, Subcommand)]
pub enum KeysAction {
    /// List API keys on your account.
    List(JsonFlag),
    /// Mint a new API key. The plaintext is printed once.
    Mint(MintKeyArgs),
    /// Revoke a key by id.
    Revoke(RevokeKeyArgs),
}

#[derive(Debug, clap::Args)]
pub struct MintKeyArgs {
    /// Operator-supplied display name.
    #[arg(long)]
    pub name: String,
    /// Wire-format scope tokens (repeat the flag, or pass a single
    /// space-delimited list). Must be a subset of your effective scopes.
    #[arg(long = "scope", value_name = "SCOPE")]
    pub scopes: Vec<String>,
    /// Optional expiry (RFC 3339, e.g. `2026-12-31T00:00:00Z`).
    #[arg(long)]
    pub expires_at: Option<DateTime<Utc>>,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct RevokeKeyArgs {
    /// The key id (e.g. `k_…`).
    pub id: String,
    #[command(flatten)]
    pub json: JsonFlag,
}

// ===== Usage / Requests =====

#[derive(Debug, clap::Args)]
pub struct UsageArgs {
    /// Lower bound (RFC 3339). Defaults to `to - 30 days`.
    #[arg(long)]
    pub from: Option<DateTime<Utc>>,
    /// Upper bound (RFC 3339). Defaults to now.
    #[arg(long)]
    pub to: Option<DateTime<Utc>>,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct RequestsArgs {
    /// Page size (server clamps to `[1, 100]`).
    #[arg(long)]
    pub limit: Option<u64>,
    /// Offset into the result set.
    #[arg(long)]
    pub offset: Option<u64>,
    #[command(flatten)]
    pub json: JsonFlag,
}

// ===== Billing =====

#[derive(Debug, Subcommand)]
pub enum BillingAction {
    /// Show the account's credit balance.
    Balance(JsonFlag),
    /// Start a Stripe checkout session for a credit top-up.
    /// Requires the `billing:write` scope.
    Checkout(CheckoutArgs),
}

#[derive(Debug, clap::Args)]
pub struct CheckoutArgs {
    /// Amount in USD cents.
    #[arg(long)]
    pub amount_cents: i64,
    #[command(flatten)]
    pub json: JsonFlag,
}

// ===== Policy =====

#[derive(Debug, Subcommand)]
pub enum PolicyAction {
    /// List policies on your account.
    List(PolicyListArgs),
    /// Fetch one policy.
    Get(IdArg),
    /// Create a policy. Spec body is read from `--spec <file|->`.
    Create(PolicyCreateArgs),
    /// Update a policy's name and / or spec.
    Update(PolicyUpdateArgs),
    /// Delete a policy.
    Delete(IdArg),
    /// Attach a policy to a principal.
    Bind(PolicyBindArgs),
    /// Detach one binding from a policy.
    Unbind(PolicyUnbindArgs),
    /// Park a policy — the engine skips it at request time.
    Disable(IdArg),
    /// Re-enable a previously disabled policy.
    Enable(IdArg),
    /// List the bindings of one policy.
    Bindings(IdArg),
    /// Preview the effective policy for a principal.
    Effective(EffectiveArgs),
    /// List every policy bound to a principal.
    #[command(name = "for-principal")]
    ForPrincipal(ForPrincipalArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum PolicyKindArg {
    Budget,
    RateLimit,
    Guardrail,
    Preset,
}

impl From<PolicyKindArg> for SdkPolicyKind {
    fn from(value: PolicyKindArg) -> Self {
        match value {
            PolicyKindArg::Budget => SdkPolicyKind::Budget,
            PolicyKindArg::RateLimit => SdkPolicyKind::RateLimit,
            PolicyKindArg::Guardrail => SdkPolicyKind::Guardrail,
            PolicyKindArg::Preset => SdkPolicyKind::Preset,
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct PolicyListArgs {
    /// Narrow the list to one kind.
    #[arg(long)]
    pub kind: Option<PolicyKindArg>,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct PolicyCreateArgs {
    /// Operator-supplied display name.
    #[arg(long)]
    pub name: String,
    /// Kind discriminator — selects which shape `--spec` must take.
    #[arg(long)]
    pub kind: PolicyKindArg,
    /// Path to a JSON file containing the flat inner spec body, or
    /// `-` to read from stdin.
    #[arg(long)]
    pub spec: PathBuf,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct PolicyUpdateArgs {
    /// The policy id.
    pub id: String,
    /// New name. Omit to leave unchanged.
    #[arg(long)]
    pub name: Option<String>,
    /// New spec. Path to a JSON file or `-` for stdin. Omit to leave
    /// unchanged.
    #[arg(long)]
    pub spec: Option<PathBuf>,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct PolicyBindArgs {
    /// The policy id.
    pub id: String,
    /// Principal kind (`namespace`, `api_key`, `oauth_token`,
    /// `oauth_client`).
    #[arg(long)]
    pub principal_type: String,
    /// Principal id — interpretation depends on `--principal-type`.
    #[arg(long)]
    pub principal_id: String,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct PolicyUnbindArgs {
    /// The policy id.
    pub id: String,
    /// The binding id (from `cloud policy bindings <id>`).
    pub binding_id: String,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct EffectiveArgs {
    /// Principal kind (`namespace`, `api_key`, `oauth_token`,
    /// `oauth_client`).
    #[arg(long)]
    pub principal_type: String,
    /// Principal id.
    #[arg(long)]
    pub principal_id: String,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct ForPrincipalArgs {
    /// Principal kind.
    pub principal_type: String,
    /// Principal id.
    pub principal_id: String,
    #[command(flatten)]
    pub json: JsonFlag,
}

// ===== Budget =====

#[derive(Debug, Subcommand)]
pub enum BudgetAction {
    /// List every budget on the account.
    List(JsonFlag),
    /// Fetch one budget.
    Get(IdArg),
    /// Create a budget.
    Create(BudgetCreateArgs),
    /// Patch a budget's fields.
    Update(BudgetUpdateArgs),
    /// Remove a budget.
    Delete(IdArg),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BudgetWindowArg {
    Day,
    Month,
    Total,
}

impl From<BudgetWindowArg> for SdkBudgetWindow {
    fn from(value: BudgetWindowArg) -> Self {
        match value {
            BudgetWindowArg::Day => SdkBudgetWindow::Day,
            BudgetWindowArg::Month => SdkBudgetWindow::Month,
            BudgetWindowArg::Total => SdkBudgetWindow::Total,
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct BudgetCreateArgs {
    /// Display name.
    #[arg(long)]
    pub name: String,
    /// Rolling-spend window.
    #[arg(long)]
    pub window: BudgetWindowArg,
    /// Spend cap in micro-USD (must be strictly positive).
    #[arg(long)]
    pub limit_micro_usd: i64,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct BudgetUpdateArgs {
    /// The budget id.
    pub id: String,
    /// New name.
    #[arg(long)]
    pub name: Option<String>,
    /// New window.
    #[arg(long)]
    pub window: Option<BudgetWindowArg>,
    /// New cap (must be strictly positive when supplied).
    #[arg(long)]
    pub limit_micro_usd: Option<i64>,
    #[command(flatten)]
    pub json: JsonFlag,
}

// ===== Preset =====

#[derive(Debug, Subcommand)]
pub enum PresetAction {
    /// List every preset on the account.
    List(JsonFlag),
    /// Fetch one preset.
    Get(IdArg),
    /// Create a preset. Each clause is supplied as a JSON file
    /// (or `-` for stdin).
    Create(PresetCreateArgs),
    /// Patch a preset's clauses. Use `--clear-*` to drop a clause.
    Update(PresetUpdateArgs),
    /// Remove a preset.
    Delete(IdArg),
}

#[derive(Debug, clap::Args)]
pub struct PresetCreateArgs {
    /// Display name.
    #[arg(long)]
    pub name: String,
    /// Optional guardrail clause (JSON file or `-`).
    #[arg(long)]
    pub guardrail: Option<PathBuf>,
    /// Optional budget clause.
    #[arg(long)]
    pub budget: Option<PathBuf>,
    /// Optional rate-limit clause.
    #[arg(long)]
    pub rate_limit: Option<PathBuf>,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct PresetUpdateArgs {
    /// The preset id.
    pub id: String,
    /// New name.
    #[arg(long)]
    pub name: Option<String>,
    /// Replace the guardrail clause (JSON file or `-`).
    #[arg(long)]
    pub guardrail: Option<PathBuf>,
    /// Replace the budget clause.
    #[arg(long)]
    pub budget: Option<PathBuf>,
    /// Replace the rate-limit clause.
    #[arg(long)]
    pub rate_limit: Option<PathBuf>,
    /// Drop the guardrail clause.
    #[arg(long)]
    pub clear_guardrail: bool,
    /// Drop the budget clause.
    #[arg(long)]
    pub clear_budget: bool,
    /// Drop the rate-limit clause.
    #[arg(long)]
    pub clear_rate_limit: bool,
    #[command(flatten)]
    pub json: JsonFlag,
}

// ===== BYOK =====

#[derive(Debug, Subcommand)]
pub enum ByokAction {
    /// List every BYOK row on the account.
    List(JsonFlag),
    /// Upsert a BYOK row. Ciphertext must be sealed by the caller
    /// against the cloud's current X25519 public key.
    Set(ByokSetArgs),
    /// Remove a BYOK row by provider id.
    Delete(ByokDeleteArgs),
}

#[derive(Debug, clap::Args)]
pub struct ByokSetArgs {
    /// Upstream provider id (e.g. `anthropic`).
    #[arg(long)]
    pub provider: String,
    /// Base64-encoded sealed-box ciphertext.
    #[arg(long)]
    pub ciphertext_b64: String,
    /// KEK id used to seal `--ciphertext-b64`. Must match the cloud's
    /// current `primary_kek_id`.
    #[arg(long)]
    pub kek_id: String,
    /// Operator-visible prefix of the underlying plaintext.
    #[arg(long)]
    pub key_prefix: String,
    /// Override API base for the provider.
    #[arg(long)]
    pub api_base: Option<String>,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct ByokDeleteArgs {
    /// Provider id (the row's `provider_name`).
    pub provider: String,
    #[command(flatten)]
    pub json: JsonFlag,
}

// ===== OAuth clients =====

#[derive(Debug, Subcommand)]
pub enum OauthClientAction {
    /// List every OAuth client on the account.
    List(JsonFlag),
    /// Register a new client.
    Register(OauthRegisterArgs),
    /// Patch one or more fields of an existing client.
    Update(OauthUpdateArgs),
    /// Remove a client.
    Delete(OauthDeleteArgs),
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ClientTypeArg {
    Confidential,
    Public,
}

impl From<ClientTypeArg> for SdkClientType {
    fn from(value: ClientTypeArg) -> Self {
        match value {
            ClientTypeArg::Confidential => SdkClientType::Confidential,
            ClientTypeArg::Public => SdkClientType::Public,
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct OauthRegisterArgs {
    /// Display name.
    #[arg(long)]
    pub name: String,
    /// Client type.
    #[arg(long = "type")]
    pub client_type: ClientTypeArg,
    /// Redirect URI. Repeat for multiple values.
    #[arg(long = "redirect-uri")]
    pub redirect_uris: Vec<String>,
    /// Wire-format scope. Repeat for multiple values.
    #[arg(long = "scope", value_name = "SCOPE")]
    pub allowed_scopes: Vec<String>,
    /// Grant type — at least one of `authorization_code`,
    /// `refresh_token`, or the RFC 8628 URN.
    #[arg(long = "grant")]
    pub allowed_grant_types: Vec<String>,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct OauthUpdateArgs {
    /// The client id (the public `client_id`, not the row id).
    pub client_id: String,
    /// New display name.
    #[arg(long)]
    pub name: Option<String>,
    /// Replace the redirect-URI list. Repeat for multiple values.
    #[arg(long = "redirect-uri")]
    pub redirect_uris: Option<Vec<String>>,
    /// Replace the scope set.
    #[arg(long = "scope", value_name = "SCOPE")]
    pub allowed_scopes: Option<Vec<String>>,
    /// Replace the grant-type list.
    #[arg(long = "grant")]
    pub allowed_grant_types: Option<Vec<String>>,
    #[command(flatten)]
    pub json: JsonFlag,
}

#[derive(Debug, clap::Args)]
pub struct OauthDeleteArgs {
    /// The client id.
    pub client_id: String,
    #[command(flatten)]
    pub json: JsonFlag,
}

// ===== Shared little-arg structs =====

#[derive(Debug, clap::Args)]
pub struct IdArg {
    /// The resource id.
    pub id: String,
    #[command(flatten)]
    pub json: JsonFlag,
}

/// Shared `--json` switch. Flatten into every leaf so the user can
/// always opt out of the text formatter.
#[derive(Debug, Default, Clone, Copy, clap::Args)]
pub struct JsonFlag {
    /// Print the response as raw JSON instead of the human-readable
    /// summary.
    #[arg(long)]
    pub json: bool,
}

// =====================================================================
// Runner
// =====================================================================

/// Entry point dispatched by `apps/bitrouter/src/main.rs`.
pub async fn run(action: CloudAction, format: crate::output::Format) -> Result<()> {
    let _ = CLOUD_FORMAT.set(format);
    let result = run_inner(action).await;
    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            print_error_hint(&err);
            Err(err.into())
        }
    }
}

async fn run_inner(action: CloudAction) -> std::result::Result<(), SdkError> {
    match action {
        CloudAction::Whoami => whoami().await,
        CloudAction::Login {
            authorization_server,
            client_id,
            scope,
        } => {
            login(LoginInputs {
                authorization_server,
                client_id,
                scope,
            })
            .await
            .map_err(SdkError::Auth)?;
            let client = client()?;
            let store = CredentialsStore::default_path().map_err(SdkError::Auth)?;
            let body = serde_json::json!({
                "signed_in": true,
                "namespace": client.namespace_id(),
                "subject": store.current().and_then(|c| c.subject.clone()),
                "scope": store.current().map(|c| c.scope.clone()),
                "credentials_path": store.path().display().to_string(),
            });
            emit(false, &body, |_| "signed in".to_string())
        }
        CloudAction::Logout {
            authorization_server,
            client_id,
        } => {
            logout(LoginInputs {
                authorization_server,
                client_id,
                scope: None,
            })
            .await
            .map_err(SdkError::Auth)?;
            emit(false, &serde_json::json!({ "signed_out": true }), |_| {
                "signed out".to_string()
            })
        }
        CloudAction::Namespace { action } => run_namespace(action).await,
        CloudAction::Keys { action } => run_keys(action).await,
        CloudAction::Usage(args) => run_usage(args).await,
        CloudAction::Requests(args) => run_requests(args).await,
        CloudAction::Billing { action } => run_billing(action).await,
        CloudAction::Policy { action } => run_policy(action).await,
        CloudAction::Budget { action } => run_budget(action).await,
        CloudAction::Preset { action } => run_preset(action).await,
        CloudAction::Byok { action } => run_byok(action).await,
        CloudAction::OauthClient { action } => run_oauth_client(action).await,
    }
}

fn client() -> std::result::Result<ManagementClient, SdkError> {
    ManagementClient::from_default_credentials()
}

async fn whoami() -> std::result::Result<(), SdkError> {
    // Offline — reads the local credentials file (works without network).
    let client = client()?;
    let store = CredentialsStore::default_path().map_err(SdkError::Auth)?;
    let scope = store.current().map(|c| c.scope.clone());
    let subject = store.current().and_then(|c| c.subject.clone());
    let signed_in = store.current().is_some();
    let body = serde_json::json!({
        "signed_in": signed_in,
        "base_url": client.base_url(),
        "namespace": client.namespace_id(),
        "scope": scope.clone(),
        "subject": subject.clone(),
        "credentials_path": store.path().display().to_string(),
    });
    emit(false, &body, |_| {
        let mut out = format!(
            "cloud base URL: {}\nnamespace:      {}",
            client.base_url(),
            client.namespace_id().unwrap_or("(none)")
        );
        if signed_in {
            if let Some(s) = &scope {
                out.push_str(&format!("\nscope:          {s}"));
            }
            if let Some(sub) = &subject {
                out.push_str(&format!("\nsubject:        {sub}"));
            }
            out.push_str(&format!("\ncredentials:    {}", store.path().display()));
        } else {
            out.push_str("\n(not signed in — run `bitrouter cloud login`)");
        }
        out
    })
}

// ----- Namespace -----

async fn run_namespace(action: NamespaceAction) -> std::result::Result<(), SdkError> {
    let client = client()?;
    match action {
        NamespaceAction::List(flag) => {
            let resp = client.list_namespaces().await?;
            let active = client.namespace_id().map(str::to_owned);
            emit(flag.json, &resp, |r| {
                format_namespace_list(r, active.as_deref())
            })
        }
        NamespaceAction::Current(flag) => {
            // Offline — the namespace is baked into the local credential.
            let nsid = client.namespace_id();
            let body = serde_json::json!({ "namespace_id": nsid });
            emit(flag.json, &body, |_| match nsid {
                Some(id) => id.to_owned(),
                None => "(no namespace — run `bitrouter cloud login`)".to_owned(),
            })
        }
    }
}

fn format_namespace_list(resp: &namespaces::NamespaceListResponse, active: Option<&str>) -> String {
    if resp.data.is_empty() {
        return "no namespaces".to_owned();
    }
    let mut out = String::new();
    for ns in &resp.data {
        let marker = if Some(ns.id.as_str()) == active {
            "  (active)"
        } else {
            ""
        };
        out.push_str(&format!("{:<28}  {}{}\n", ns.id, ns.name, marker));
    }
    out
}

// ----- Keys -----

async fn run_keys(action: KeysAction) -> std::result::Result<(), SdkError> {
    let client = client()?;
    match action {
        KeysAction::List(flag) => emit(flag.json, &client.list_keys().await?, format_keys_list),
        KeysAction::Mint(args) => {
            let body = keys::MintApiKeyRequest {
                display_name: args.name,
                scopes: split_scope_args(&args.scopes),
                expires_at: args.expires_at,
            };
            let resp = client.mint_key(&body).await?;
            emit(args.json.json, &resp, format_mint_key)
        }
        KeysAction::Revoke(args) => {
            let resp = client.revoke_key(&args.id).await?;
            emit(args.json.json, &resp, |r| format!("revoked: {}", r.revoked))
        }
    }
}

fn format_keys_list(resp: &keys::ApiKeyListResponse) -> String {
    if resp.data.is_empty() {
        return "no keys".to_owned();
    }
    let mut out = String::new();
    for k in &resp.data {
        out.push_str(&format!(
            "{:<24}  {:<24}  scopes=[{}]  prefix={}\n",
            k.id,
            k.display_name,
            k.scopes.join(", "),
            k.key_prefix,
        ));
    }
    out
}

fn format_mint_key(resp: &keys::MintApiKeyResponse) -> String {
    format!(
        "id:           {id}\nname:         {name}\nprefix:       {prefix}\nscopes:       [{scopes}]\ntoken:        {token}   (shown once — save it now)\n",
        id = resp.id,
        name = resp.display_name,
        prefix = resp.key_prefix,
        scopes = resp.scopes.join(", "),
        token = resp.token,
    )
}

// ----- Usage / requests -----

async fn run_usage(args: UsageArgs) -> std::result::Result<(), SdkError> {
    let client = client()?;
    let resp = client
        .usage_aggregate(&usage::UsageQuery {
            from: args.from,
            to: args.to,
        })
        .await?;
    emit(args.json.json, &resp, |r| {
        format!(
            "window:            {from} → {to}\nspend (micro-USD): {spend}\nprompt tokens:     {pt}\ncompletion tokens: {ct}\nrequests:          {rc}\n",
            from = r.from.to_rfc3339(),
            to = r.to.to_rfc3339(),
            spend = r.spend_micro_usd,
            pt = r.prompt_tokens,
            ct = r.completion_tokens,
            rc = r.request_count,
        )
    })
}

async fn run_requests(args: RequestsArgs) -> std::result::Result<(), SdkError> {
    let client = client()?;
    let resp = client
        .list_requests(&usage::RequestsQuery {
            limit: args.limit,
            offset: args.offset,
        })
        .await?;
    emit(args.json.json, &resp, format_requests)
}

fn format_requests(resp: &usage::RequestsResponse) -> String {
    if resp.data.is_empty() {
        return "no requests".to_owned();
    }
    let mut out = String::new();
    for r in &resp.data {
        out.push_str(&format!(
            "{}  {:<10}  {:<24}  {:>10} µUSD  prompt={:<6} completion={:<6}\n",
            r.created_at.to_rfc3339(),
            r.status,
            r.model_id.clone().unwrap_or_else(|| "—".into()),
            r.final_charge_micro_usd
                .map(|v| v.to_string())
                .unwrap_or_else(|| "—".into()),
            r.prompt_tokens,
            r.completion_tokens,
        ));
    }
    out.push_str(&format!("\nlimit={} offset={}\n", resp.limit, resp.offset));
    out
}

// ----- Billing -----

async fn run_billing(action: BillingAction) -> std::result::Result<(), SdkError> {
    let client = client()?;
    match action {
        BillingAction::Balance(flag) => {
            let resp = client.billing_balance().await?;
            emit(flag.json, &resp, |r| {
                format!(
                    "balance:    {} {} (micro-USD)\npending:    {} {}\navailable:  {} {}\n",
                    r.balance_micro_usd,
                    r.currency,
                    r.pending_debits_micro_usd,
                    r.currency,
                    r.available_micro_usd,
                    r.currency,
                )
            })
        }
        BillingAction::Checkout(args) => {
            let resp = client
                .create_checkout_session(&billing::CheckoutSessionRequest {
                    amount_cents: args.amount_cents,
                })
                .await?;
            emit(args.json.json, &resp, |r| {
                format!("session id: {}\ncheckout url: {}\n", r.id, r.url)
            })
        }
    }
}

// ----- Policy -----

async fn run_policy(action: PolicyAction) -> std::result::Result<(), SdkError> {
    let client = client()?;
    match action {
        PolicyAction::List(args) => {
            let resp = client
                .list_policies(&policies::ListPoliciesQuery {
                    kind: args.kind.map(Into::into),
                })
                .await?;
            emit(args.json.json, &resp, format_policy_list)
        }
        PolicyAction::Get(args) => {
            let resp = client.get_policy(&args.id).await?;
            emit(args.json.json, &resp, format_policy_one)
        }
        PolicyAction::Create(args) => {
            let spec = read_json_input(&args.spec)?;
            let resp = client
                .create_policy(&policies::CreatePolicyRequest {
                    name: args.name,
                    kind: args.kind.into(),
                    spec,
                })
                .await?;
            emit(args.json.json, &resp, format_policy_one)
        }
        PolicyAction::Update(args) => {
            let spec = match args.spec.as_ref() {
                Some(p) => Some(read_json_input(p)?),
                None => None,
            };
            let resp = client
                .update_policy(
                    &args.id,
                    &policies::UpdatePolicyRequest {
                        name: args.name,
                        spec,
                    },
                )
                .await?;
            emit(args.json.json, &resp, format_policy_one)
        }
        PolicyAction::Delete(args) => {
            let resp = client.delete_policy(&args.id).await?;
            emit(args.json.json, &resp, |r| {
                format!("deleted: {}\n", r.deleted)
            })
        }
        PolicyAction::Bind(args) => {
            let resp = client
                .bind_policy(
                    &args.id,
                    &policies::BindPolicyRequest {
                        principal_type: args.principal_type,
                        principal_id: args.principal_id,
                    },
                )
                .await?;
            emit(args.json.json, &resp, |r| {
                format!("binding id: {}\n", r.binding_id)
            })
        }
        PolicyAction::Unbind(args) => {
            let resp = client.unbind_policy(&args.id, &args.binding_id).await?;
            emit(args.json.json, &resp, |r| {
                format!("unbound: {}\n", r.unbound)
            })
        }
        PolicyAction::Disable(args) => {
            let resp = client.disable_policy(&args.id).await?;
            emit(args.json.json, &resp, |r| {
                format!("disabled: {}\n", r.disabled)
            })
        }
        PolicyAction::Enable(args) => {
            let resp = client.enable_policy(&args.id).await?;
            emit(args.json.json, &resp, |r| {
                format!("disabled: {}\n", r.disabled)
            })
        }
        PolicyAction::Bindings(args) => {
            let resp = client.list_policy_bindings(&args.id).await?;
            emit(args.json.json, &resp, format_binding_list)
        }
        PolicyAction::Effective(args) => {
            let resp = client
                .effective_policy(&policies::EffectivePolicyQuery {
                    principal_type: args.principal_type,
                    principal_id: args.principal_id,
                })
                .await?;
            emit(args.json.json, &resp, format_effective_policy)
        }
        PolicyAction::ForPrincipal(args) => {
            let resp = client
                .list_principal_policies(&args.principal_type, &args.principal_id)
                .await?;
            emit(args.json.json, &resp, format_policy_list)
        }
    }
}

fn format_policy_list(resp: &policies::PolicyListResponse) -> String {
    if resp.data.is_empty() {
        return "no policies".to_owned();
    }
    let mut out = String::new();
    for p in &resp.data {
        let disabled = if p.disabled_at.is_some() {
            " (disabled)"
        } else {
            ""
        };
        out.push_str(&format!(
            "{:<24}  {:<24}  kind={}{}\n",
            p.id,
            p.name,
            p.kind.as_str(),
            disabled,
        ));
    }
    out
}

fn format_policy_one(p: &policies::PolicyEnvelope) -> String {
    let mut out = format!(
        "id:    {}\nname:  {}\nkind:  {}\n",
        p.id,
        p.name,
        p.kind.as_str(),
    );
    if let Some(d) = p.disabled_at {
        out.push_str(&format!("disabled_at: {}\n", d.to_rfc3339()));
    }
    out.push_str(&format!(
        "spec:\n{}\n",
        serde_json::to_string_pretty(&p.spec).unwrap_or_else(|_| "{}".to_owned())
    ));
    out
}

fn format_binding_list(resp: &policies::BindingListResponse) -> String {
    if resp.data.is_empty() {
        return "no bindings".to_owned();
    }
    let mut out = String::new();
    for b in &resp.data {
        out.push_str(&format!(
            "{:<24}  policy={:<24}  principal={}:{}\n",
            b.id, b.policy_id, b.principal_type, b.principal_id,
        ));
    }
    out
}

fn format_effective_policy(p: &policies::EffectivePolicy) -> String {
    let mut out = String::new();
    out.push_str(&format!("budgets:    {} entries\n", p.budgets.len()));
    out.push_str(&format!("rate_limits: {} entries\n", p.rate_limits.len()));
    out.push_str(&format!(
        "guardrail:  {}\n",
        if p.guardrail.is_some() { "set" } else { "—" }
    ));
    if !p.budgets.is_empty() || !p.rate_limits.is_empty() || p.guardrail.is_some() {
        out.push_str("\nfull body:\n");
        if let Ok(pretty) = serde_json::to_string_pretty(p) {
            out.push_str(&pretty);
            out.push('\n');
        }
    }
    out
}

// ----- Budget -----

async fn run_budget(action: BudgetAction) -> std::result::Result<(), SdkError> {
    let client = client()?;
    match action {
        BudgetAction::List(flag) => {
            let resp = client.list_budgets().await?;
            emit(flag.json, &resp, format_budget_list)
        }
        BudgetAction::Get(args) => {
            let resp = client.get_budget(&args.id).await?;
            emit(args.json.json, &resp, format_budget_one)
        }
        BudgetAction::Create(args) => {
            let resp = client
                .create_budget(&budgets::CreateBudgetRequest {
                    name: args.name,
                    window: args.window.into(),
                    limit_micro_usd: args.limit_micro_usd,
                })
                .await?;
            emit(args.json.json, &resp, format_budget_one)
        }
        BudgetAction::Update(args) => {
            let resp = client
                .update_budget(
                    &args.id,
                    &budgets::UpdateBudgetRequest {
                        name: args.name,
                        window: args.window.map(Into::into),
                        limit_micro_usd: args.limit_micro_usd,
                    },
                )
                .await?;
            emit(args.json.json, &resp, format_budget_one)
        }
        BudgetAction::Delete(args) => {
            let resp = client.delete_budget(&args.id).await?;
            emit(args.json.json, &resp, |r| {
                format!("deleted: {}\n", r.deleted)
            })
        }
    }
}

fn format_budget_list(resp: &budgets::BudgetListResponse) -> String {
    if resp.data.is_empty() {
        return "no budgets".to_owned();
    }
    let mut out = String::new();
    for b in &resp.data {
        out.push_str(&format!(
            "{:<24}  {:<24}  window={:?}  limit={} µUSD{}\n",
            b.id,
            b.name,
            b.window,
            b.limit_micro_usd,
            if b.disabled_at.is_some() {
                " (disabled)"
            } else {
                ""
            },
        ));
    }
    out
}

fn format_budget_one(b: &budgets::BudgetEnvelope) -> String {
    let mut out = format!(
        "id:              {}\nname:            {}\nwindow:          {:?}\nlimit (µUSD):    {}\n",
        b.id, b.name, b.window, b.limit_micro_usd,
    );
    if let Some(d) = b.disabled_at {
        out.push_str(&format!("disabled_at:     {}\n", d.to_rfc3339()));
    }
    out
}

// ----- Preset -----

async fn run_preset(action: PresetAction) -> std::result::Result<(), SdkError> {
    let client = client()?;
    match action {
        PresetAction::List(flag) => {
            let resp = client.list_presets().await?;
            emit(flag.json, &resp, format_preset_list)
        }
        PresetAction::Get(args) => {
            let resp = client.get_preset(&args.id).await?;
            emit(args.json.json, &resp, format_preset_one)
        }
        PresetAction::Create(args) => {
            let body = presets::CreatePresetRequest {
                name: args.name,
                guardrail: opt_json_input(args.guardrail.as_ref())?,
                budget: opt_json_input(args.budget.as_ref())?,
                rate_limit: opt_json_input(args.rate_limit.as_ref())?,
            };
            let resp = client.create_preset(&body).await?;
            emit(args.json.json, &resp, format_preset_one)
        }
        PresetAction::Update(args) => {
            let body = presets::UpdatePresetRequest {
                name: args.name,
                guardrail: opt_json_input(args.guardrail.as_ref())?,
                budget: opt_json_input(args.budget.as_ref())?,
                rate_limit: opt_json_input(args.rate_limit.as_ref())?,
                clear_guardrail: args.clear_guardrail,
                clear_budget: args.clear_budget,
                clear_rate_limit: args.clear_rate_limit,
            };
            let resp = client.update_preset(&args.id, &body).await?;
            emit(args.json.json, &resp, format_preset_one)
        }
        PresetAction::Delete(args) => {
            let resp = client.delete_preset(&args.id).await?;
            emit(args.json.json, &resp, |r| {
                format!("deleted: {}\n", r.deleted)
            })
        }
    }
}

fn format_preset_list(resp: &presets::PresetListResponse) -> String {
    if resp.data.is_empty() {
        return "no presets".to_owned();
    }
    let mut out = String::new();
    for p in &resp.data {
        let clauses = [
            ("guardrail", p.guardrail.is_some()),
            ("budget", p.budget.is_some()),
            ("rate_limit", p.rate_limit.is_some()),
        ]
        .iter()
        .filter(|(_, set)| *set)
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(",");
        out.push_str(&format!(
            "{:<24}  {:<24}  clauses=[{}]{}\n",
            p.id,
            p.name,
            clauses,
            if p.disabled_at.is_some() {
                " (disabled)"
            } else {
                ""
            },
        ));
    }
    out
}

fn format_preset_one(p: &presets::PresetEnvelope) -> String {
    let mut out = format!("id:    {}\nname:  {}\n", p.id, p.name);
    if let Some(d) = p.disabled_at {
        out.push_str(&format!("disabled_at: {}\n", d.to_rfc3339()));
    }
    for (label, value) in [
        ("guardrail", &p.guardrail),
        ("budget", &p.budget),
        ("rate_limit", &p.rate_limit),
    ] {
        if let Some(v) = value {
            out.push_str(&format!(
                "{label}:\n{}\n",
                serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".to_owned())
            ));
        }
    }
    out
}

// ----- BYOK -----

async fn run_byok(action: ByokAction) -> std::result::Result<(), SdkError> {
    let client = client()?;
    match action {
        ByokAction::List(flag) => {
            let resp = client.list_byok_keys().await?;
            emit(flag.json, &resp, format_byok_list)
        }
        ByokAction::Set(args) => {
            let body = byok::UpsertByokKeyRequest {
                provider_name: args.provider,
                ciphertext_b64: args.ciphertext_b64,
                kek_id: args.kek_id,
                key_prefix: args.key_prefix,
                api_base: args.api_base,
            };
            let resp = client.upsert_byok_key(&body).await?;
            emit(args.json.json, &resp, |r| {
                format!(
                    "provider:  {}\nkek_id:    {}\nprefix:    {}\n",
                    r.provider_name, r.kek_id, r.key_prefix,
                )
            })
        }
        ByokAction::Delete(args) => {
            let resp = client.delete_byok_key(&args.provider).await?;
            emit(args.json.json, &resp, |r| {
                format!("deleted: {}\n", r.deleted)
            })
        }
    }
}

fn format_byok_list(resp: &byok::ByokKeyListResponse) -> String {
    if resp.data.is_empty() {
        return "no byok keys".to_owned();
    }
    let mut out = String::new();
    for k in &resp.data {
        out.push_str(&format!(
            "{:<20}  prefix={:<10}  kek={:<24}  last_used={}\n",
            k.provider_name,
            k.key_prefix,
            k.kek_id,
            k.last_used_at
                .map(|t| t.to_rfc3339())
                .unwrap_or_else(|| "—".into()),
        ));
    }
    out
}

// ----- OAuth clients -----

async fn run_oauth_client(action: OauthClientAction) -> std::result::Result<(), SdkError> {
    let client = client()?;
    match action {
        OauthClientAction::List(flag) => {
            let resp = client.list_oauth_clients().await?;
            emit(flag.json, &resp, format_oauth_client_list)
        }
        OauthClientAction::Register(args) => {
            let body = oauth_clients::RegisterOauthClientRequest {
                client_name: args.name,
                client_type: args.client_type.into(),
                redirect_uris: args.redirect_uris,
                allowed_scopes: args.allowed_scopes,
                allowed_grant_types: args.allowed_grant_types,
            };
            let resp = client.register_oauth_client(&body).await?;
            emit(args.json.json, &resp, format_oauth_client_register)
        }
        OauthClientAction::Update(args) => {
            let body = oauth_clients::UpdateOauthClientRequest {
                client_name: args.name,
                redirect_uris: args.redirect_uris,
                allowed_scopes: args.allowed_scopes,
                allowed_grant_types: args.allowed_grant_types,
            };
            let resp = client.update_oauth_client(&args.client_id, &body).await?;
            emit(args.json.json, &resp, format_oauth_client_envelope)
        }
        OauthClientAction::Delete(args) => {
            let resp = client.delete_oauth_client(&args.client_id).await?;
            emit(args.json.json, &resp, |r| {
                format!("deleted: {}\n", r.deleted)
            })
        }
    }
}

fn format_oauth_client_list(resp: &oauth_clients::OauthClientListResponse) -> String {
    if resp.data.is_empty() {
        return "no clients".to_owned();
    }
    let mut out = String::new();
    for c in &resp.data {
        out.push_str(&format!(
            "{:<24}  {:<24}  type={:?}  grants=[{}]  scopes=[{}]\n",
            c.client_id,
            c.client_name,
            c.client_type,
            c.allowed_grant_types.join(","),
            c.allowed_scopes.join(","),
        ));
    }
    out
}

fn format_oauth_client_envelope(c: &oauth_clients::OauthClientEnvelope) -> String {
    format!(
        "id:           {}\nclient_id:    {}\nname:         {}\ntype:         {:?}\nredirect_uris: {:?}\nscopes:       [{}]\ngrants:       [{}]\n",
        c.id,
        c.client_id,
        c.client_name,
        c.client_type,
        c.redirect_uris,
        c.allowed_scopes.join(", "),
        c.allowed_grant_types.join(", "),
    )
}

fn format_oauth_client_register(c: &oauth_clients::RegisterOauthClientResponse) -> String {
    let mut out = format!(
        "id:           {}\nclient_id:    {}\nname:         {}\ntype:         {:?}\nredirect_uris: {:?}\nscopes:       [{}]\ngrants:       [{}]\n",
        c.id,
        c.client_id,
        c.client_name,
        c.client_type,
        c.redirect_uris,
        c.allowed_scopes.join(", "),
        c.allowed_grant_types.join(", "),
    );
    if let Some(secret) = c.client_secret.as_deref() {
        out.push_str(&format!(
            "client_secret: {secret}   (shown once — save it now)\n"
        ));
    }
    out
}

// =====================================================================
// Output + error helpers
// =====================================================================

/// The global output format, set once at the start of [`run`]. `emit` consults
/// it so cloud leaves default to JSON (agent-native) and switch to the human
/// formatter only under the global `--human` flag.
static CLOUD_FORMAT: std::sync::OnceLock<crate::output::Format> = std::sync::OnceLock::new();

/// Whether a cloud leaf should print JSON. JSON by default; the global
/// `--human` flag selects the human formatter; a per-leaf `--json` still forces
/// JSON.
fn effective_json(leaf_json: bool) -> bool {
    if leaf_json {
        return true;
    }
    !matches!(CLOUD_FORMAT.get(), Some(crate::output::Format::Human))
}

fn emit<T, F>(leaf_json: bool, value: &T, fmt: F) -> std::result::Result<(), SdkError>
where
    T: serde::Serialize,
    F: FnOnce(&T) -> String,
{
    if effective_json(leaf_json) {
        let pretty = serde_json::to_string_pretty(value)?;
        println!("{pretty}");
    } else {
        let text = fmt(value);
        if !text.is_empty() {
            // Trim a single trailing newline so we don't double-space
            // when the formatter already appended one.
            let trimmed = text.strip_suffix('\n').unwrap_or(&text);
            println!("{trimmed}");
        }
    }
    Ok(())
}

fn read_json_input(path: &std::path::Path) -> std::result::Result<serde_json::Value, SdkError> {
    use std::io::Read;
    let raw = if path == std::path::Path::new("-") {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).map_err(|e| {
            SdkError::Auth(anyhow::Error::new(e).context("reading JSON from stdin"))
        })?;
        buf
    } else {
        std::fs::read_to_string(path).map_err(|e| {
            SdkError::Auth(
                anyhow::Error::new(e).context(format!("reading JSON from {}", path.display())),
            )
        })?
    };
    serde_json::from_str::<serde_json::Value>(&raw).map_err(SdkError::from)
}

fn opt_json_input(
    path: Option<&PathBuf>,
) -> std::result::Result<Option<serde_json::Value>, SdkError> {
    match path {
        Some(p) => Ok(Some(read_json_input(p)?)),
        None => Ok(None),
    }
}

/// `--scope` accepts both repeated flags and a single space-delimited
/// list per flag — mirroring how the same value is handled by
/// `bitrouter cloud login --scope`. Empty tokens are dropped.
fn split_scope_args(scopes: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in scopes {
        for token in raw.split_whitespace() {
            if !token.is_empty() {
                out.push(token.to_owned());
            }
        }
    }
    out
}

fn print_error_hint(err: &SdkError) {
    match err {
        SdkError::NotSignedIn => {
            eprintln!();
            eprintln!("  Sign in first:");
            eprintln!("    bitrouter cloud login");
        }
        SdkError::Forbidden {
            missing_scope: Some(scope),
            ..
        } => {
            eprintln!();
            eprintln!("  This command requires the scope: {scope}");
            if let Some(extended) = suggested_scope(scope) {
                eprintln!("  Re-run `bitrouter cloud login --scope \"{extended}\"` to add it.");
            } else {
                eprintln!(
                    "  Re-run `bitrouter cloud login --scope \"<your current scope> {scope}\"`."
                );
            }
        }
        _ => {}
    }
}

/// Best-effort: read the locally stored scope and append `missing` to
/// it, so the hint we print is copy-pasteable. Returns `None` when the
/// credentials file is unreadable.
fn suggested_scope(missing: &str) -> Option<String> {
    let path = default_credentials_path().ok()?;
    let store = CredentialsStore::load(&path).ok()?;
    let current = store.current()?.scope.clone();
    if current.split_whitespace().any(|s| s == missing) {
        // Already present — probably the server is rejecting something
        // we can't auto-fix. Don't suggest a duplicate.
        Some(current)
    } else {
        Some(format!("{current} {missing}"))
    }
}
