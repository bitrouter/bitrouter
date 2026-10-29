use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::operator::*;
use serde_json::json;

use super::*;
use crate::gateway::shared::{crypto::DeploymentKey, schema, store};

fn plan(principal_id: &str) -> BootstrapPlan {
    BootstrapPlan {
        resources: vec![ResourcePut {
            resource_pool_id: "pool".into(),
            expected_revision: None,
            concurrent_attempts: 4,
        }],
        tenants: vec![TenantPut {
            tenant_id: "tenant".into(),
            expected_revision: None,
            limits: TenantLimits {
                concurrent_executions: 4,
                requests_per_minute: 100,
                resource_pools: ["pool".into()].into(),
            },
        }],
        principals: vec![PrincipalPut {
            principal_id: principal_id.into(),
            tenant_id: "tenant".into(),
            expected_authorization_revision: None,
            key_hash: Some(crate::auth::keys::hash_key(
                &uuid::Uuid::new_v4().to_string(),
            )),
            active: true,
            expires_at_ms: None,
            requests_per_minute: None,
            policy_id: None,
        }],
        credentials: vec![CredentialImport {
            provider: "fixture".into(),
            account: "default".into(),
            resource_pool_id: "pool".into(),
            expected_version: None,
            credential: CredentialInput::ApiKey {
                value: "fixture-bootstrap-secret".into(),
            },
        }],
        configuration: Publication {
            format: 1,
            routing: json!({"providers":{"fixture":{"api_base":"https://example.com/v1","models":[{"id":"model"}]}}}),
            policies: vec![],
            policy_lock: None,
            credentials: vec![CredentialBinding {
                provider: "fixture".into(),
                account: "default".into(),
                credential_provider: "fixture".into(),
                credential_account: "default".into(),
            }],
            require_known_pricing: false,
        },
    }
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL test database"]
async fn postgres_atomic_bootstrap() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), fixture())
        .await
        .context("bootstrap fixture deadline exceeded")?
}

async fn fixture() -> Result<()> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
    let first_db = store::connect(&url, 3, Duration::from_secs(3)).await?;
    let second_db = store::connect(&url, 3, Duration::from_secs(3)).await?;
    let result = async {
        let namespace = format!("bootstrap-{}", uuid::Uuid::new_v4());
        let first = Store::new(first_db.clone(), &namespace, Duration::from_secs(3))?;
        let second = Store::new(second_db.clone(), &namespace, Duration::from_secs(3))?;
        crate::db::run_migrations(first.database()).await?;
        schema::initialize(&first).await?;
        let deployment_key = rand::random();
        let authority = Authority::new(first, DeploymentKey::from_bytes(deployment_key)?);
        let peer = Authority::new(second, DeploymentKey::from_bytes(deployment_key)?);
        let actor = Actor::new(
            "bootstrap-operator".into(),
            [
                Grant::Read,
                Grant::PolicyWrite,
                Grant::CredentialWrite,
                Grant::Publish,
                Grant::GlobalPrincipalWrite,
            ]
            .into(),
        )?;
        let input = plan(&format!("bootstrap-key-{}", uuid::Uuid::new_v4()));
        let (left, right) = tokio::join!(
            initialize(authority.clone(), &actor, "root", &input),
            initialize(peer.clone(), &actor, "root", &input)
        );
        let receipt = left?;
        ensure!(receipt == right? && receipt.operations.len() == 5);
        ensure!(
            receipt.revision
                == Revision {
                    config_generation: 1,
                    key_generation: 1
                }
        );
        ensure!(!serde_json::to_string(&receipt)?.contains("fixture-bootstrap-secret"));
        let operator = Operator::new(authority.clone());
        operator
            .apply(
                &actor,
                &Mutation {
                    operation_id: "later-publication".into(),
                    command: Command::Publish(PublicationPut {
                        expected_generation: 1,
                        configuration: input.configuration.clone(),
                    }),
                },
            )
            .await?;
        ensure!(initialize(peer.clone(), &actor, "root", &input).await? == receipt);
        ensure!(authority.head().await?.config_generation == 2);
        let mut changed = input.clone();
        changed.tenants[0].limits.requests_per_minute = 101;
        expect_code(
            initialize(peer, &actor, "root", &changed).await,
            ErrorCode::Conflict,
        )?;

        let fresh_store = Store::new(
            first_db.clone(),
            &format!("bootstrap-{}", uuid::Uuid::new_v4()),
            Duration::from_secs(3),
        )?;
        let fresh = Authority::new(fresh_store, DeploymentKey::from_bytes(deployment_key)?);
        let limited = Actor::new(
            actor.id.clone(),
            [Grant::PolicyWrite, Grant::CredentialWrite, Grant::Publish].into(),
        )?;
        expect_code(
            initialize(fresh.clone(), &limited, "root", &input).await,
            ErrorCode::Forbidden,
        )?;
        ensure!(
            fresh
                .store()
                .get::<serde_json::Value>(Kind::Authority, "head")
                .await?
                .is_none()
        );
        // A collision at the global principal step happens after resource and
        // tenant writes. The whole seed, keys and all intermediate receipts roll
        // back, while the original namespace/global principal remains intact.
        expect_code(
            initialize(fresh.clone(), &actor, "root", &input).await,
            ErrorCode::Conflict,
        )?;
        for (kind, key) in [
            (Kind::Authority, "head"),
            (Kind::Authority, "keys/1"),
            (Kind::Authority, BOOTSTRAP_RECORD),
            (Kind::Resource, "pool"),
            (Kind::Tenant, "tenant"),
        ] {
            ensure!(
                fresh
                    .store()
                    .get::<serde_json::Value>(kind, key)
                    .await?
                    .is_none()
            );
        }
        for operation in mutations("root", &input)? {
            let key = super::super::operation_key(&actor.id, &operation.operation_id)?;
            ensure!(
                fresh
                    .store()
                    .get::<serde_json::Value>(Kind::OperatorReceipt, &key)
                    .await?
                    .is_none()
            );
        }
        let repaired = plan(&format!("bootstrap-key-{}", uuid::Uuid::new_v4()));
        initialize(fresh.clone(), &actor, "root", &repaired).await?;
        ensure!(
            fresh.head().await?.config_generation == 1
                && authority.head().await?.config_generation == 2
        );
        let foreign_store = Store::new(
            first_db.clone(),
            &format!("bootstrap-{}", uuid::Uuid::new_v4()),
            Duration::from_secs(3),
        )?;
        let foreign = Authority::new(foreign_store, DeploymentKey::from_bytes(deployment_key)?);
        foreign.initialize().await?;
        expect_code(
            initialize(foreign.clone(), &actor, "root", &input).await,
            ErrorCode::Conflict,
        )?;
        ensure!(foreign.head().await?.config_generation == 0);
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let first_closed = first_db.close().await;
    let second_closed = second_db.close().await;
    result?;
    first_closed?;
    second_closed?;
    Ok(())
}

fn expect_code<T>(result: Result<T>, code: ErrorCode) -> Result<()> {
    let error = result.err().context("bootstrap unexpectedly succeeded")?;
    ensure!(matches!(error.downcast_ref::<Failure>(), Some(Failure(actual)) if *actual == code));
    Ok(())
}
