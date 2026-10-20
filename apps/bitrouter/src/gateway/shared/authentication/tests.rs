//! Real shared credentials through the production authentication registry.

use anyhow::{Context, ensure};

use super::*;
use crate::gateway::model::target::{AuthScheme, CredentialPriority};
use crate::gateway::providers::auth::credentials::OAuthToken;
use crate::gateway::providers::auth::store::OAuthRefresher;
use crate::gateway::shared::{
    authority::Authority, credentials::CredentialStamp, crypto::DeploymentKey, engine::Engine,
    store::Store,
};

fn target(provider: &str, protocol: ApiProtocol) -> ModelTarget {
    ModelTarget {
        provider_name: provider.into(),
        service_id: "fixture".into(),
        api_protocol: protocol,
        api_base: "https://example.invalid/v1".into(),
        api_key: String::new(),
        credential_priority: CredentialPriority::Explicit,
        account_label: None,
        auth_scheme: AuthScheme::XApiKey,
        compatibility: Default::default(),
    }
}

fn request() -> anyhow::Result<reqwest::Request> {
    Ok(reqwest::Client::new()
        .post("https://example.invalid/v1/fixture")
        .json(&serde_json::json!({}))
        .build()?)
}

pub(crate) async fn exercise(first: &Store, second: &Store) -> anyhow::Result<()> {
    let authority = Authority::new(first.clone(), DeploymentKey::from_bytes([9; 32])?);
    let engine = Engine::join(authority.clone(), 30_000).await?;
    let pool = uuid::Uuid::new_v4().to_string();
    engine.set_resource(&pool, None, 4).await?;
    let credentials = Credentials::new(authority);
    let remote = Credentials::new(Authority::new(
        second.clone(),
        DeploymentKey::from_bytes([9; 32])?,
    ));
    let slot = CredentialKey {
        provider: pool.clone(),
        account: "imported".into(),
    };
    let first_stamp = credentials
        .import(
            &slot,
            &pool,
            None,
            &Credential::ApiKey {
                value: "first-key".into(),
            },
        )
        .await?;
    let bindings = ["fixture", "alias"].map(|provider| CredentialBinding {
        provider: provider.into(),
        account: "default".into(),
        credential_provider: slot.provider.clone(),
        credential_account: slot.account.clone(),
    });
    let publication = super::super::publication::Configuration {
        format: 1,
        routing: serde_json::json!({"providers": {
            "fixture": {"api_base": "https://example.invalid/v1", "auto_discover": false},
            "alias": {"derives": "fixture", "api_base": "https://example.invalid/v1", "auto_discover": false}
        }}),
        policies: vec![],
        policy_lock: None,
        credentials: bindings.to_vec(),
        require_known_pricing: false,
    };
    let config = publication.prepare()?;
    let registry = build(&config, &bindings, credentials.clone())?;
    let other = build(&config, &bindings, remote.clone())?;
    for protocol in [
        ApiProtocol::ChatCompletions,
        ApiProtocol::Messages,
        ApiProtocol::Responses,
        ApiProtocol::Decisions,
    ] {
        let target = target("alias", protocol);
        let applied = registry
            .lookup("alias")
            .context("alias missing")?
            .apply_with_authority(request()?, &target)
            .await?;
        ensure!(applied.shared_credential()? == Some(first_stamp.clone()));
        let header = if target.api_protocol == ApiProtocol::Messages {
            "x-api-key"
        } else {
            "authorization"
        };
        let expected = if header == "x-api-key" {
            "first-key"
        } else {
            "Bearer first-key"
        };
        ensure!(
            applied
                .headers()
                .get(header)
                .context("authentication missing")?
                == expected
        );
    }
    let selected = target("fixture", ApiProtocol::ChatCompletions);
    let applier = registry.lookup("fixture").context("provider missing")?;
    let old_request = applier.apply_with_authority(request()?, &selected).await?;
    let replaced = remote
        .import(
            &slot,
            &pool,
            Some(first_stamp.version),
            &Credential::ApiKey {
                value: "second-key".into(),
            },
        )
        .await?;
    // Inspection never reloads a newer version and attributes it to old wire auth.
    ensure!(old_request.shared_credential()? == Some(first_stamp.clone()));
    let tx = first.begin().await?;
    ensure!(credentials.fence(&tx, &first_stamp).await.is_err());
    tx.rollback().await?;
    let new_request = other
        .lookup("alias")
        .context("remote alias missing")?
        .apply_with_authority(request()?, &target("alias", ApiProtocol::ChatCompletions))
        .await?;
    ensure!(new_request.shared_credential()? == Some(replaced));
    ensure!(
        new_request
            .headers()
            .get("authorization")
            .context("rotated authentication missing")?
            == "Bearer second-key"
    );

    let mut forbidden = selected.clone();
    forbidden.api_key = "inline-override".into();
    ensure!(
        applier
            .apply_with_authority(request()?, &forbidden)
            .await
            .is_err()
    );
    forbidden.api_key.clear();
    forbidden.account_label = Some("unbound-account".into());
    ensure!(
        applier
            .apply_with_authority(request()?, &forbidden)
            .await
            .is_err()
    );
    oauth_capture(&credentials, &slot, &pool).await?;
    subscription_aliases(&credentials, &slot, &pool).await?;
    Ok(())
}

async fn subscription_aliases(
    credentials: &Credentials,
    slot: &CredentialKey,
    pool: &str,
) -> anyhow::Result<()> {
    for mechanism in [
        "claude-code",
        "openai-codex",
        "supergrok",
        "github-copilot",
        "anthropic",
    ] {
        let publication = super::super::publication::Configuration {
            format: 1,
            routing: serde_json::json!({"providers": {
                mechanism: {"api_base": "https://example.invalid/v1", "auto_discover": false},
                "middle": {"derives": mechanism, "api_base": "https://example.invalid/v1", "auto_discover": false},
                "alias": {"derives": "middle", "api_base": "https://example.invalid/v1", "auto_discover": false}
            }}),
            policies: vec![],
            policy_lock: None,
            require_known_pricing: false,
            credentials: [mechanism, "middle", "alias"]
                .map(|provider| CredentialBinding {
                    provider: provider.into(),
                    account: "default".into(),
                    credential_provider: slot.provider.clone(),
                    credential_account: slot.account.clone(),
                })
                .to_vec(),
        };
        let prepared = publication.prepare()?;
        ensure!(prepared.config.providers["alias"].derives.is_none());
        ensure!(prepared.authentication["alias"] == mechanism);
        ensure!(prepared.authentication["middle"] == mechanism);
        let registry = build(&prepared, &publication.credentials, credentials.clone())?;
        let applier = registry
            .lookup("alias")
            .context("subscription alias missing")?;
        let protocol = if mechanism == "claude-code" || mechanism == "anthropic" {
            ApiProtocol::Messages
        } else {
            ApiProtocol::Responses
        };
        let selected = target("alias", protocol);
        let mut body = serde_json::json!({"model": "fixture", "input": []});
        applier.prepare_body(&mut body, &selected).await?;
        ensure!(applier.requires_streaming(&selected) == (mechanism == "openai-codex"));
        if mechanism == "openai-codex" {
            ensure!(
                body["store"] == false
                    && body["include"] == serde_json::json!(["reasoning.encrypted_content"])
            );
        }
        if mechanism == "claude-code" {
            ensure!(
                body["system"]
                    .as_array()
                    .is_some_and(|blocks| !blocks.is_empty())
            );
        }
        if mechanism == "github-copilot" {
            // The production exchange URL must never receive test credentials.
            // Existing counted mock exchange tests cover its selected-store flow.
            continue;
        }
        let previous = credentials.load(slot).await?.0;
        let secret = if mechanism == "anthropic" {
            Credential::ApiKey {
                value: "alias-key".into(),
            }
        } else {
            Credential::Oauth(OAuthToken {
                access_token: "alias-access".into(),
                expires_at: 0,
                refresh_token: None,
            })
        };
        let imported = credentials
            .import(slot, pool, Some(previous.version), &secret)
            .await?;
        let applied = applier.apply_with_authority(request()?, &selected).await?;
        ensure!(applied.shared_credential()? == Some(imported));
        let (name, value) = if mechanism == "anthropic" {
            ("x-api-key", "alias-key")
        } else {
            ("authorization", "Bearer alias-access")
        };
        ensure!(
            applied
                .headers()
                .get(name)
                .context("alias wire authentication missing")?
                == value
        );
    }
    Ok(())
}

struct Refresh;
#[async_trait]
impl OAuthRefresher for Refresh {
    async fn refresh(&self, _: &OAuthToken) -> Result<OAuthToken> {
        // Yield in the independently owned OAuth task; task locals do not follow
        // tokio::spawn, while the acquired transaction's observation does.
        tokio::task::yield_now().await;
        Ok(OAuthToken {
            access_token: "fresh-access".into(),
            expires_at: 0,
            refresh_token: Some("fresh-refresh".into()),
        })
    }
}

async fn oauth_capture(
    credentials: &Credentials,
    slot: &CredentialKey,
    pool: &str,
) -> anyhow::Result<()> {
    let previous = credentials.load(slot).await?.0;
    let imported = credentials
        .import(
            slot,
            pool,
            Some(previous.version),
            &Credential::Oauth(OAuthToken {
                access_token: "expired-access".into(),
                expires_at: 1,
                refresh_token: Some("fixture-refresh".into()),
            }),
        )
        .await?;
    let store: Arc<dyn CredentialStore> = Arc::new(BoundStore {
        credentials: credentials.clone(),
        mechanism: "claude-code".into(),
        accounts: [("default".into(), slot.clone())].into(),
    });
    let applier = SharedApplier {
        mechanism: "claude-code".into(),
        inner: Arc::new(providers::claude_code::ClaudeCodeAuthApplier::new(
            OAuthSession::new(store, Arc::new(Refresh), Duration::from_secs(30)),
        )),
    };
    let applied = applier
        .apply_with_authority(request()?, &target("claude-alias", ApiProtocol::Messages))
        .await?;
    let stamp = applied
        .shared_credential()?
        .context("rotated stamp missing")?;
    ensure!(stamp.version == imported.version + 1 && stamp.pool_id == pool);
    ensure!(
        applied
            .headers()
            .get("authorization")
            .context("OAuth bearer missing")?
            == "Bearer fresh-access"
    );
    ensure!(credentials.load(slot).await?.0 == stamp);
    Ok(())
}

fn stamp() -> CredentialStamp {
    CredentialStamp {
        id: "credential-id".into(),
        pool_id: "resource-pool".into(),
        version: 1,
    }
}

#[test]
fn shared_wire_proof_survives_only_non_authority_mutations() -> anyhow::Result<()> {
    for changed in [
        "authorization",
        "destination",
        "chatgpt-account-id",
        "x-api-key",
        "x-goog-api-key",
    ] {
        let mut request = request()?;
        request.headers_mut().insert(
            "authorization",
            reqwest::header::HeaderValue::from_static("Bearer original"),
        );
        let mut applied = AppliedAuth::unproven(request).bind_shared_credential(stamp());
        applied.headers_mut().insert(
            "x-bitrouter-request-id",
            reqwest::header::HeaderValue::from_static("correlation"),
        );
        ensure!(applied.shared_credential()? == Some(stamp()));
        if changed == "destination" {
            applied.url_mut().set_path("/other");
        } else {
            applied.headers_mut().insert(
                reqwest::header::HeaderName::from_static(changed),
                reqwest::header::HeaderValue::from_static("changed"),
            );
        }
        ensure!(
            applied.shared_credential().is_err(),
            "mutated {changed} retained resource authority"
        );
    }
    Ok(())
}

#[tokio::test]
async fn authentication_observations_are_isolated_and_conflicts_stick() -> anyhow::Result<()> {
    let first = Observation::default();
    let second = Observation::default();
    ensure!(Observation::current().is_none());
    first
        .scope(async {
            let current = Observation::current().context("missing observation")?;
            tokio::spawn(async move { current.record(&stamp()) }).await??;
            second
                .scope(async {
                    ensure!(second.finish().is_err());
                    Ok::<_, anyhow::Error>(())
                })
                .await?;
            Ok::<_, anyhow::Error>(())
        })
        .await?;
    ensure!(first.finish()? == stamp());
    let mut changed = stamp();
    changed.version += 1;
    ensure!(first.record(&changed).is_err());
    ensure!(first.record(&stamp()).is_err());
    ensure!(first.finish().is_err());
    ensure!(second.finish().is_err());
    Ok(())
}
