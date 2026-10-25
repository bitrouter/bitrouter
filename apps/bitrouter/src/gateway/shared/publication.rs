//! Immutable, encrypted routing publications, separate from process configuration.

use std::collections::{BTreeSet, HashMap};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::gateway::providers::auth::store::{CredentialKey, DEFAULT_ACCOUNT};
use crate::policy::Policy;
use crate::policy_lock::PolicyLock;

use super::authority::{Authority, Head};
use super::credentials::Credentials;
use super::engine::Rejection;
use super::store::{Kind, NewRecord, Store, Stored};
use crate::runtime_crypto::SealedSecret;

const MAX_PUBLICATION_BYTES: usize = 1024 * 1024;
const MAX_PROVIDERS: usize = 256;
const MAX_DERIVATION_DEPTH: usize = 32;

/// A route account can alias an existing credential slot. Resource identity belongs
/// to that slot, so aliases cannot multiply the upstream account's concurrency.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CredentialBinding {
    pub provider: String,
    pub account: String,
    pub credential_provider: String,
    pub credential_account: String,
}

impl CredentialBinding {
    pub fn slot(&self) -> CredentialKey {
        CredentialKey {
            provider: self.credential_provider.clone(),
            account: self.credential_account.clone(),
        }
    }
}

/// Only encrypted instances are persisted. Header defaults may contain secrets,
/// so this document deliberately has no Debug implementation or public projection.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Configuration {
    pub format: u32,
    /// Explicit routing fields; listener/database/worker settings are not accepted.
    pub routing: serde_json::Value,
    /// Complete policy documents, never paths to files on individual workers.
    #[serde(default)]
    pub policies: Vec<serde_json::Value>,
    pub policy_lock: Option<PolicyLock>,
    pub credentials: Vec<CredentialBinding>,
    #[serde(default)]
    pub require_known_pricing: bool,
}

pub(super) struct PreparedConfiguration {
    pub config: Config,
    pub policies: Vec<Policy>,
    /// Resolved from the validated source graph, before local config parsing
    /// erases `derives`. Credential slot names do not select auth mechanisms.
    pub authentication: HashMap<String, String>,
}

impl Configuration {
    /// Preparation performs no environment, filesystem, credential or provider I/O.
    /// Every replica validates the same complete input before exposing it.
    pub fn prepare(&self) -> Result<PreparedConfiguration> {
        ensure!(self.format == 1, "unsupported shared configuration format");
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_PUBLICATION_BYTES,
            "shared configuration exceeds byte limit"
        );
        let mut fields = self
            .routing
            .as_object()
            .context("shared routing must be an object")?
            .clone();
        for field in fields.keys() {
            ensure!(
                matches!(
                    field.as_str(),
                    "providers"
                        | "models"
                        | "routers"
                        | "checkers"
                        | "presets"
                        | "variants"
                        | "upstream"
                        | "trajectory"
                        | "continuation"
                        | "policy"
                ),
                "field '{field}' is not part of shared routing authority"
            );
        }
        fields.insert("inherit_defaults".into(), serde_json::json!(false));
        fields.insert("registry".into(), serde_json::json!({"enabled": false}));
        fields.insert(
            "server".into(),
            serde_json::json!({
                "skip_auth": false, "require_known_pricing": self.require_known_pricing
            }),
        );
        validate_derivation_bounds(&fields)?;
        // YAML diagnostics can include surrounding secret header values. Drop the
        // source error entirely rather than retaining it under anyhow::Context.
        let config = crate::config::parse_resolved(&serde_json::to_string(&fields)?)
            .map_err(|_| anyhow::anyhow!("invalid shared routing configuration"))?;
        ensure!(
            config.policy.path.is_none(),
            "shared routing requires an embedded policy lock, not policy.path"
        );
        ensure!(
            config.checkers.is_empty(),
            "shared gateway publications cannot load native extensions"
        );
        ensure!(
            self.credentials.len() <= 256,
            "too many shared credential bindings"
        );
        let mut accounts = BTreeSet::new();
        for (id, provider) in &config.providers {
            ensure!(
                provider.headers.iter().all(|(name, policy)| {
                    !matches!(policy, crate::config::ProviderHeaderConfig::Policy(policy) if policy.passthrough)
                        || super::http::fingerprint::allows_header(name)
                }),
                "shared provider passthrough is outside the request identity contract"
            );
            ensure!(
                provider
                    .models
                    .iter()
                    .all(|model| !model.compatibility.chat_completions.google_extensions),
                "shared Google Chat replay authority is not supported; disable google_extensions or use local mode"
            );
            ensure!(
                !provider.auto_discover && !provider.api_base.is_empty(),
                "shared provider '{id}' requires an explicit endpoint and disabled discovery"
            );
            ensure!(
                provider.api_key.is_empty()
                    && provider
                        .accounts
                        .iter()
                        .all(|account| account.api_key.is_empty()),
                "shared provider '{id}' must reference imported credentials"
            );
            ensure!(
                provider.rate_limits.is_empty()
                    && provider
                        .models
                        .iter()
                        .all(|model| model.rate_limits.is_none()),
                "shared provider rate limits require shared resource policy"
            );
            if provider.accounts.is_empty() {
                accounts.insert((id.as_str(), DEFAULT_ACCOUNT));
            } else {
                for account in &provider.accounts {
                    ensure!(
                        !account.label.is_empty()
                            && accounts.insert((id.as_str(), account.label.as_str())),
                        "shared provider accounts require unique explicit labels"
                    );
                }
            }
        }
        let mut bindings = BTreeSet::new();
        for binding in &self.credentials {
            for id in [
                &binding.provider,
                &binding.account,
                &binding.credential_provider,
                &binding.credential_account,
            ] {
                ensure!(
                    !id.is_empty() && id.len() <= 256,
                    "invalid shared credential binding"
                );
            }
            ensure!(
                bindings.insert((binding.provider.as_str(), binding.account.as_str())),
                "duplicate shared credential binding"
            );
        }
        ensure!(
            bindings == accounts,
            "shared credential bindings must cover exactly the configured accounts"
        );
        ensure!(
            self.policies.len() <= 256,
            "too many shared access policies"
        );
        let mut policies = Vec::with_capacity(self.policies.len());
        let mut ids = BTreeSet::new();
        for document in &self.policies {
            for field in document
                .as_object()
                .context("shared policy must be an object")?
                .keys()
            {
                ensure!(
                    matches!(
                        field.as_str(),
                        "id" | "allowed_models"
                            | "denied_models"
                            | "max_spend_micro_usd"
                            | "expires_at"
                            | "allowed_tools"
                            | "max_requests_per_minute"
                    ),
                    "unknown shared access-policy field"
                );
            }
            let policy: Policy = serde_json::from_value(document.clone())
                .map_err(|_| anyhow::anyhow!("invalid shared access policy"))?;
            ensure!(
                !policy.id.is_empty() && policy.id.len() <= 256 && ids.insert(policy.id.clone()),
                "shared access policies require unique bounded IDs"
            );
            ensure!(
                policy.max_spend_micro_usd.is_none() && policy.max_requests_per_minute.is_none(),
                "shared access policy cannot use local spend/rate counters; configure shared admission limits"
            );
            policies.push(policy);
        }
        if let Some(document) = &self.policy_lock {
            crate::policy_lock::validate_document(document)?;
            crate::policy_lock::validate_for_config(&config, document)?;
        } else {
            ensure!(
                crate::policy_lock::bound_policy_names(&config).is_empty(),
                "shared policy bindings require an embedded policy lock"
            );
        }
        let authentication = config
            .providers
            .keys()
            .map(|provider| {
                super::authentication::mechanism(&self.routing, provider)
                    .map(|mechanism| (provider.clone(), mechanism))
            })
            .collect::<Result<_>>()?;
        Ok(PreparedConfiguration {
            config,
            policies,
            authentication,
        })
    }
}

/// Check the raw graph before the local parser recursively resolves derivations.
/// Document bytes alone do not bound stack use for a chain of small providers.
fn validate_derivation_bounds(fields: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    let Some(providers) = fields.get("providers") else {
        return Ok(());
    };
    let providers = providers
        .as_object()
        .context("shared providers must be an object")?;
    ensure!(
        providers.len() <= MAX_PROVIDERS,
        "too many shared providers"
    );
    for start in providers.keys() {
        let mut current = Some(start.as_str());
        let mut visited = BTreeSet::new();
        while let Some(id) = current {
            ensure!(
                visited.len() < MAX_DERIVATION_DEPTH,
                "shared provider derivation depth exceeds limit"
            );
            ensure!(visited.insert(id), "cyclic shared provider derivation");
            let provider = providers
                .get(id)
                .and_then(serde_json::Value::as_object)
                .context("invalid shared provider derivation target")?;
            current = match provider.get("derives") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(parent)) => Some(parent),
                _ => anyhow::bail!("invalid shared provider derivation reference"),
            };
        }
    }
    Ok(())
}

impl Authority {
    /// Commit a validated immutable document and its head in the same transaction.
    /// The exclusive head fence orders publication after all older dispatch commits.
    #[cfg(test)]
    pub async fn publish(
        &self,
        expected_generation: u64,
        configuration: &Configuration,
    ) -> Result<Head> {
        let prepared = self.prepare_publication(expected_generation, configuration)?;
        self.store()
            .retry(|| async {
                let tx = self.store().begin().await?;
                let mut head = self.fence_exclusive(&tx).await?;
                self.publish_in(&tx, &mut head, &prepared).await?;
                tx.commit().await?;
                Ok(head.value)
            })
            .await
    }

    pub fn prepare_publication(
        &self,
        expected_generation: u64,
        configuration: &Configuration,
    ) -> Result<PreparedPublication> {
        configuration.prepare()?;
        let generation = expected_generation
            .checked_add(1)
            .context("configuration generation exhausted")?;
        let record = format!("configuration/{generation}");
        let sealed = self.seal(&record, generation, &serde_json::to_vec(configuration)?)?;
        let slots = configuration
            .credentials
            .iter()
            .map(CredentialBinding::slot)
            .collect();
        Ok(PreparedPublication {
            expected_generation,
            generation,
            record,
            sealed,
            slots,
        })
    }

    /// The caller acquired the exclusive head before its operation receipt.
    pub async fn publish_in(
        &self,
        tx: &sea_orm::DatabaseTransaction,
        head: &mut Stored<Head>,
        input: &PreparedPublication,
    ) -> Result<()> {
        ensure!(
            head.value.config_generation == input.expected_generation,
            Rejection::AuthorityChanged
        );
        let credentials = Credentials::new(self.clone());
        let mut stamps = Vec::new();
        for slot in &input.slots {
            let (stamp, _) = credentials.load_in(tx, slot).await?;
            stamps.push(stamp);
        }
        stamps.sort_by(|left, right| left.id.cmp(&right.id));
        stamps.dedup();
        for stamp in &stamps {
            credentials.fence(tx, stamp).await?;
        }
        let now = Store::now(tx).await?;
        ensure!(
            self.store()
                .create(
                    tx,
                    NewRecord {
                        kind: Kind::Authority,
                        key: &input.record,
                        status: "retained",
                        due_at_ms: None,
                        now_ms: now,
                        value: &input.sealed,
                    }
                )
                .await?,
            "configuration generation already exists"
        );
        head.value.config_generation = input.generation;
        self.store().save(tx, head, "active", None, now).await?;
        Ok(())
    }

    /// Load the exact immutable version identified by a previously read head.
    /// The admission fence still rejects it if a newer generation was published.
    pub async fn configuration(&self, head: &Head) -> Result<Configuration> {
        self.validate_head(head)?;
        ensure!(
            head.config_generation > 0,
            "shared namespace has no published configuration"
        );
        let record = format!("configuration/{}", head.config_generation);
        let stored: Stored<SealedSecret> = self
            .store()
            .bounded(async {
                self.store()
                    .get(Kind::Authority, &record)
                    .await?
                    .context("shared configuration generation is missing")
            })
            .await?;
        ensure!(
            stored.revision()? == 0,
            "immutable shared configuration was modified"
        );
        let plaintext = self.open(&record, head.config_generation, &stored.value)?;
        let configuration: Configuration = serde_json::from_slice(&plaintext)
            .map_err(|_| anyhow::anyhow!("invalid shared configuration encoding"))?;
        configuration.prepare()?;
        Ok(configuration)
    }
}

/// Validated and encrypted before opening the short publication transaction.
pub(super) struct PreparedPublication {
    expected_generation: u64,
    generation: u64,
    record: String,
    sealed: SealedSecret,
    slots: Vec<CredentialKey>,
}
