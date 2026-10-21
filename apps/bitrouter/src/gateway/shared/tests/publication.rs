//! Immutable publication and independent-replica configuration regressions.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::json;

use super::super::authority::Authority;
use super::super::credentials::Credentials;
use super::super::crypto::{DeploymentKey, SealedSecret};
use super::super::engine::Engine;
use super::super::publication::{Configuration, CredentialBinding};
use super::super::store::{Kind, Store, Stored};
use crate::gateway::providers::auth::credentials::Credential;

fn candidate() -> Configuration {
    Configuration {
        format: 1,
        routing: json!({"providers": {"fixture": {
            "api_base": "https://example.com/v1",
            "headers": {"x-fixture": "${LITERAL_NOT_AN_ENV_REFERENCE}"},
            "models": [{"id": "fixture-model"}]
        }}}),
        policies: vec![json!({"id":"models", "allowed_models":["fixture-model"]})],
        policy_lock: None,
        credentials: vec![CredentialBinding {
            provider: "fixture".into(),
            account: "default".into(),
            credential_provider: "fixture-account".into(),
            credential_account: "primary".into(),
        }],
        require_known_pricing: false,
    }
}

#[test]
fn publication_restricts_header_passthrough_to_fixed_request_identity() -> Result<()> {
    for name in ["user-agent", "OpenAI-Organization", "x-codex-turn-metadata"] {
        let mut document = candidate();
        document.routing["providers"]["fixture"]["headers"] =
            json!({name: {"passthrough":true,"default":"fixed-fallback"}});
        document.prepare()?;
    }
    for name in [
        "x-unlisted-routing",
        "traceparent",
        "authorization",
        "x-bitrouter-request-id",
    ] {
        let mut document = candidate();
        document.routing["providers"]["fixture"]["headers"] =
            json!({name: {"passthrough":true,"default":"private-sentinel"}});
        let error = document
            .prepare()
            .err()
            .context("accepted unbound passthrough")?;
        ensure!(!format!("{error:#}").contains("private-sentinel"));
        ensure!(
            error.to_string().contains("passthrough")
                || error.to_string() == "invalid shared routing configuration"
        );
    }
    let mut inherited = candidate();
    inherited.routing["providers"]["base"] = json!({
        "api_base":"https://example.com/v1","auto_discover":false,
        "headers":{"x-unlisted-routing":{"passthrough":true}}
    });
    inherited.routing["providers"]["fixture"]["derives"] = json!("base");
    inherited.credentials.push(CredentialBinding {
        provider: "base".into(),
        account: "default".into(),
        credential_provider: "fixture-account".into(),
        credential_account: "primary".into(),
    });
    let error = inherited
        .prepare()
        .err()
        .context("accepted inherited unbound passthrough")?;
    ensure!(error.to_string().contains("passthrough"));
    Ok(())
}

#[test]
fn publication_has_no_ambient_authority() -> Result<()> {
    let document = candidate();
    let prepared = document.prepare()?;
    ensure!(!prepared.config.inherit_defaults && !prepared.config.registry.enabled);
    ensure!(!prepared.config.server.skip_auth && prepared.policies.len() == 1);
    let mut unsupported = document.clone();
    unsupported.routing["providers"]["fixture"]["models"][0]["compatibility"] =
        json!({"chat_completions": {"google_extensions": true}});
    let rejection = unsupported
        .prepare()
        .err()
        .context("accepted unsupported shared Google replay")?;
    ensure!(
        rejection
            .to_string()
            .contains("shared Google Chat replay authority is not supported")
    );
    ensure!(
        matches!(prepared.config.providers["fixture"].headers.get("x-fixture"),
        Some(crate::config::ProviderHeaderConfig::Static(value))
            if value == "${LITERAL_NOT_AN_ENV_REFERENCE}")
    );
    for field in [
        "database",
        "gateway_state",
        "server",
        "registry",
        "inherit_defaults",
        "agents",
        "mcp",
        "server_tools",
        "plugins",
        "decision_model",
    ] {
        let mut invalid = document.clone();
        invalid.routing[field] = json!({});
        ensure!(
            invalid.prepare().is_err(),
            "accepted process-owned field {field}"
        );
    }
    let mut invalid = document.clone();
    invalid.routing["policy"] = json!({"path": "secret-local-policy.yaml"});
    ensure!(invalid.prepare().is_err());
    let mut invalid = document.clone();
    invalid.routing["checkers"] = json!({"unregistered": {"native": {"revision":"1"}}});
    ensure!(invalid.prepare().is_err());
    for field in [
        "max_spend_micro_usd",
        "max_requests_per_minute",
        "misspelled_limit",
    ] {
        let mut invalid = document.clone();
        invalid.policies[0][field] = json!(1);
        ensure!(
            invalid.prepare().is_err(),
            "accepted unsupported policy field {field}"
        );
    }
    let mut invalid = document.clone();
    invalid.routing["providers"]["fixture"]["api_key"] = json!("unimported-secret");
    ensure!(invalid.prepare().is_err());
    let mut invalid = document.clone();
    invalid.routing["providers"]["fixture"]["auto_discover"] = json!(true);
    ensure!(invalid.prepare().is_err());
    let mut invalid = document.clone();
    invalid.credentials.clear();
    ensure!(invalid.prepare().is_err());
    let mut invalid = document.clone();
    invalid.credentials.push(document.credentials[0].clone());
    ensure!(invalid.prepare().is_err());
    let mut invalid = document;
    invalid.policies.push(invalid.policies[0].clone());
    ensure!(invalid.prepare().is_err());
    Ok(())
}

#[test]
fn publication_errors_do_not_expose_document_values() -> Result<()> {
    let secret = "shared-publication-diagnostic-sentinel";
    let mut document = candidate();
    document.routing["providers"]["fixture"]["headers"]["x-fixture"] = json!(secret);
    document.routing["providers"]["fixture"]["active"] = json!(secret);
    let error = document
        .prepare()
        .err()
        .context("malformed provider was accepted")?;
    ensure!(!format!("{error:#}").contains(secret));
    ensure!(!format!("{error:?}").contains(secret));
    let mut document = candidate();
    document.policies[0]["allowed_models"] = json!(secret);
    let error = document
        .prepare()
        .err()
        .context("malformed policy was accepted")?;
    ensure!(!format!("{error:#}").contains(secret));
    ensure!(!format!("{error:?}").contains(secret));
    Ok(())
}

#[test]
fn publication_bounds_derivation_before_recursive_resolution() -> Result<()> {
    for count in [33, 257, 4_000] {
        let mut document = candidate();
        let providers = (0..count)
            .map(|index| {
                (
                    format!("provider-{index}"),
                    if index == 0 {
                        json!({"api_base":"https://example.com/v1"})
                    } else {
                        json!({"derives":format!("provider-{}", index - 1)})
                    },
                )
            })
            .collect::<serde_json::Map<_, _>>();
        document.routing["providers"] = json!(providers);
        ensure!(serde_json::to_vec(&document)?.len() < 1024 * 1024);
        let error = document
            .prepare()
            .err()
            .context("oversized derivation was accepted")?;
        ensure!(format!("{error}").contains(if count > 256 {
            "too many shared providers"
        } else {
            "derivation depth"
        }));
    }
    let mut document = candidate();
    document.routing["providers"]["fixture"]["derives"] = json!("fixture");
    let error = document
        .prepare()
        .err()
        .context("cyclic derivation was accepted")?;
    ensure!(format!("{error}").contains("cyclic shared provider derivation"));
    let mut document = candidate();
    document.routing["providers"]["derived"] = json!({
        "api_base": "https://example.com/v1", "derives":"fixture"
    });
    let mut binding = document.credentials[0].clone();
    binding.provider = "derived".into();
    document.credentials.push(binding);
    let prepared = document.prepare()?;
    ensure!(prepared.config.providers["derived"].models[0].id == "fixture-model");
    Ok(())
}

pub(super) async fn exercise(first: &Store, second: &Store) -> Result<()> {
    // Separate namespace: earlier engine fixtures intentionally use generation 0.
    let namespace = format!("publication-{}", uuid::Uuid::new_v4());
    let deployment_key = rand::random();
    let a = Authority::new(
        Store::new(first.database().clone(), &namespace, Duration::from_secs(3))?,
        DeploymentKey::from_bytes(deployment_key)?,
    );
    let b = Authority::new(
        Store::new(
            second.database().clone(),
            &namespace,
            Duration::from_secs(3),
        )?,
        DeploymentKey::from_bytes(deployment_key)?,
    );
    let head = a.initialize().await?;
    ensure!(b.configuration(&head).await.is_err());
    let candidate = candidate();
    ensure!(a.publish(0, &candidate).await.is_err());
    ensure!(b.head().await?.config_generation == 0);
    let engine = Engine::join(a.clone(), 30_000).await?;
    engine.set_resource("physical-account", None, 2).await?;
    Credentials::new(a.clone())
        .import(
            &candidate.credentials[0].slot(),
            "physical-account",
            None,
            &Credential::ApiKey {
                value: "publication-fixture-secret".into(),
            },
        )
        .await?;
    let (left, right) = tokio::join!(a.publish(0, &candidate), b.publish(0, &candidate));
    ensure!(
        left.is_ok() != right.is_ok(),
        "competing publications both committed or both failed"
    );
    let first_head = b.head().await?;
    ensure!(first_head.config_generation == 1);
    let first_document = b.configuration(&first_head).await?;
    ensure!(first_document.routing == candidate.routing);
    let sealed: Stored<SealedSecret> = b
        .store()
        .get(Kind::Authority, "configuration/1")
        .await?
        .context("published row missing")?;
    let encoded = serde_json::to_string(&sealed.value)?;
    ensure!(
        !encoded.contains("LITERAL_NOT_AN_ENV_REFERENCE") && !encoded.contains("fixture-model")
    );

    let mut next = candidate.clone();
    next.routing["providers"]["alias"] = next.routing["providers"]["fixture"].clone();
    let mut alias = next.credentials[0].clone();
    alias.provider = "alias".into();
    next.credentials.push(alias);
    next.policies[0]["denied_models"] = json!(["blocked"]);
    let second_head = a.publish(1, &next).await?;
    ensure!(second_head.config_generation == 2);
    ensure!(a.publish(1, &candidate).await.is_err());
    let fresh = b.configuration(&second_head).await?;
    ensure!(fresh.prepare()?.policies[0].denied_models == ["blocked"]);
    let credentials = Credentials::new(b.clone());
    let (original, _) = credentials.load(&fresh.credentials[0].slot()).await?;
    let (alias, _) = credentials.load(&fresh.credentials[1].slot()).await?;
    ensure!(original == alias && original.pool_id == "physical-account");
    ensure!(
        a.configuration(&first_head).await?.routing == candidate.routing,
        "later publication changed the retained older request snapshot"
    );
    let rotated = b.rotate_correlation(second_head.key_generation).await?;
    ensure!(
        rotated.config_generation == 2 && a.configuration(&rotated).await?.routing == next.routing
    );

    // A failed write cannot leave the head pointing to an unusable document.
    let mut invalid = next.clone();
    invalid.credentials[0].credential_account = "missing".into();
    ensure!(b.publish(2, &invalid).await.is_err());
    ensure!(a.head().await?.config_generation == 2);
    let wrong_key = Authority::new(
        a.store().clone(),
        DeploymentKey::from_bytes(rand::random())?,
    );
    ensure!(wrong_key.configuration(&second_head).await.is_err());
    ensure!(wrong_key.publish(2, &next).await.is_err());

    // Corrupting a retained row is detected, not silently installed by a replica.
    let tx = a.store().begin().await?;
    let mut retained: Stored<SealedSecret> = a
        .store()
        .lock(&tx, Kind::Authority, "configuration/2")
        .await?;
    a.store()
        .save(&tx, &mut retained, "retained", None, Store::now(&tx).await?)
        .await?;
    tx.commit().await?;
    ensure!(b.configuration(&second_head).await.is_err());
    engine.drain().await?;
    Ok(())
}
