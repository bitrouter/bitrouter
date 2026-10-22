//! Read-only authentication must not contend on a refresh lease.

use super::*;
use crate::gateway::model::target::{AuthScheme, CredentialPriority};
use crate::gateway::shared::{
    authority::Authority, crypto::DeploymentKey, engine::Engine, schema, store::Store,
    test_database::EmptyDatabase,
};

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_static_reads_share_one_pool() -> anyhow::Result<()> {
    exercise(false).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_static_reads_cross_independent_pools() -> anyhow::Result<()> {
    exercise(true).await
}

async fn exercise(independent: bool) -> anyhow::Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let first = database.connect(4, Duration::from_secs(3)).await?;
        let second = if independent {
            database.connect(4, Duration::from_secs(3)).await?
        } else {
            first.clone()
        };
        let store = Store::new(first, "static-reads", Duration::from_secs(3))?;
        schema::initialize(&store).await?;
        let authority = Authority::new(store.clone(), DeploymentKey::fixture()?);
        authority.initialize().await?;
        let engine = Engine::join(authority.clone(), 30_000).await?;
        engine.set_resource("pool", None, 32).await?;
        let credentials = Credentials::new(authority);
        let remote = Credentials::new(Authority::new(
            Store::new(second, "static-reads", Duration::from_secs(3))?,
            DeploymentKey::fixture()?,
        ));
        let slot = CredentialKey {
            provider: "stored".into(),
            account: "default".into(),
        };
        let stamp = credentials
            .import(
                &slot,
                "pool",
                None,
                &Credential::ApiKey {
                    value: "fixture-static-key".into(),
                },
            )
            .await?;
        let publication = super::super::publication::Configuration {
            format: 1,
            routing: serde_json::json!({"providers": {
                "fixture": {"api_base":"https://example.invalid/v1", "auto_discover":false},
                "anthropic": {"api_base":"https://example.invalid/v1", "auto_discover":false}
            }}),
            policies: vec![],
            policy_lock: None,
            credentials: ["fixture", "anthropic"]
                .map(|provider| CredentialBinding {
                    provider: provider.into(),
                    account: "default".into(),
                    credential_provider: slot.provider.clone(),
                    credential_account: slot.account.clone(),
                })
                .to_vec(),
            require_known_pricing: false,
        };
        let prepared = publication.prepare()?;
        let registries = [
            build(&prepared, &publication.credentials, credentials.clone())?,
            build(&prepared, &publication.credentials, remote.clone())?,
        ];
        // Hold an unused read lease to make the former overlap deterministic.
        // No refresh intent exists and no provider network request is sent.
        let mut lease = credentials.begin(&slot).await?;
        let calls = (0..16).map(|index| {
            let registry = &registries[index % 2];
            let expected = stamp.clone();
            async move {
                for provider in ["fixture", "anthropic"] {
                    let applied = apply(registry, provider).await?;
                    ensure!(applied.shared_credential()? == Some(expected.clone()));
                    ensure!(
                        applied.headers().get("x-api-key").context("missing key")?
                            == "fixture-static-key"
                    );
                }
                Ok::<(), anyhow::Error>(())
            }
        });
        let results = futures::future::join_all(calls).await;
        lease.release().await?;
        for result in results {
            result?;
        }
        ensure!(remote.load(&slot).await?.0 == stamp);

        let captured = [
            apply(&registries[0], "fixture").await?,
            apply(&registries[0], "anthropic").await?,
        ];
        let mut refresh = credentials.begin(&slot).await?;
        refresh.prepare_refresh().await?;
        for provider in ["fixture", "anthropic"] {
            ensure!(
                apply(&registries[1], provider).await.is_err(),
                "static read bypassed unresolved refresh intent"
            );
        }
        let tx = store.begin().await?;
        ensure!(credentials.fence(&tx, &stamp).await.is_err());
        tx.rollback().await?;
        let replaced = remote
            .import(
                &slot,
                "pool",
                Some(stamp.version),
                &Credential::ApiKey {
                    value: "replacement-key".into(),
                },
            )
            .await?;
        // Explicit replacement resolves uncertainty, but the former refresh
        // owner cannot overwrite it or release another owner's lease.
        refresh.stage(Credential::ApiKey {
            value: "stale-key".into(),
        });
        ensure!(refresh.commit().await == Err(StoreError::Conflict));
        ensure!(refresh.release().await == Err(StoreError::Conflict));
        for old in captured {
            ensure!(old.shared_credential()? == Some(stamp.clone()));
            ensure!(
                old.headers().get("x-api-key").context("missing old key")? == "fixture-static-key"
            );
        }
        let tx = store.begin().await?;
        ensure!(remote.fence(&tx, &stamp).await.is_err());
        remote.fence(&tx, &replaced).await?;
        tx.rollback().await?;
        for provider in ["fixture", "anthropic"] {
            let fresh = apply(&registries[1], provider).await?;
            ensure!(fresh.shared_credential()? == Some(replaced.clone()));
            ensure!(
                fresh
                    .headers()
                    .get("x-api-key")
                    .context("missing replacement key")?
                    == "replacement-key"
            );
        }
        Ok(())
    })
    .await
    .context("static authentication concurrency deadline")
    .and_then(|result| result);
    database.finish(result).await
}

async fn apply(registry: &AuthAppliers, provider: &str) -> anyhow::Result<AppliedAuth> {
    let target = ModelTarget {
        provider_name: provider.into(),
        service_id: "fixture".into(),
        api_protocol: ApiProtocol::Messages,
        api_base: "https://example.invalid/v1".into(),
        api_key: String::new(),
        credential_priority: CredentialPriority::Explicit,
        account_label: None,
        auth_scheme: AuthScheme::XApiKey,
        compatibility: Default::default(),
    };
    Ok(registry
        .lookup(provider)
        .context("missing static applier")?
        .apply_with_authority(
            reqwest::Request::new(reqwest::Method::POST, target.api_base.parse()?),
            &target,
        )
        .await?)
}
