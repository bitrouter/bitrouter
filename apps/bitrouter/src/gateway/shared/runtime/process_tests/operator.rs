//! SDK calls cross the actual stock process boundary and shared authority.

use super::*;
use bitrouter_ai::gateway::operator::{
    Command as MutationCommand, Mutation, OperationLookup, ResourcePut,
};
use bitrouter_sdk::operator::OperatorClient;

const TOKEN: &str = "fixture-operator-process-0123456789abcdef";

#[tokio::test]
#[ignore = "requires PostgreSQL and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_operator_receipts_cross_replicas_and_survive_readiness_loss() -> Result<()> {
    let mut fixture = Fixture::new(true).await?;
    let result = async {
        let a_operator = available_address()?;
        let a = fixture.spawn_with_operator("serve", available_address()?, &[], Some((a_operator, TOKEN))).await?;
        fixture.child(a)?.ready().await?;
        let b_operator = available_address()?;
        let b = fixture.spawn_with_operator("serve", available_address()?, &[], Some((b_operator, TOKEN))).await?;
        fixture.child(b)?.ready().await?;
        let client_a = OperatorClient::new(&format!("http://{a_operator}"), TOKEN)?;
        let client_b = OperatorClient::new(&format!("http://{b_operator}"), TOKEN)?;
        let resource = "pool/東京%&+?";
        let mutation = Mutation { operation_id: "..".into(), command: MutationCommand::PutResource(ResourcePut {
            resource_pool_id: resource.into(), expected_revision: None, concurrent_attempts: 3,
        }) };
        let receipt = client_a.apply(&mutation).await?;
        ensure!(receipt.namespace == fixture.authority.store().namespace());
        ensure!(matches!(client_b.operation("..").await?, OperationLookup::Committed {receipt:other} if *other == receipt));
        ensure!(client_b.apply(&mutation).await? == receipt);
        let policy = client_b.resource(resource).await?;
        ensure!(policy.resource_pool_id == resource && policy.concurrent_attempts == 3);
        let http = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(2)).build()?;
        for index in [a, b] {
            let address = fixture.child(index)?.address;
            let response = http.get(format!("http://{address}/operator/v1/revision"))
                .bearer_auth(TOKEN).send().await?;
            ensure!(response.status() == StatusCode::NOT_FOUND, "operator route leaked onto inference listener");
        }

        // Inject a modified immutable publication atomically with its new head.
        // This is a storage fault, not an input accepted by the operator API.
        // Administrative reads must remain live while inference fails closed.
        let head = fixture.authority.head().await?;
        let configuration = fixture.authority.configuration(&head).await?;
        let prepared = fixture.authority.prepare_publication(head.config_generation, &configuration)?;
        let store = fixture.authority.store();
        let tx = store.begin().await?;
        let mut failed = fixture.authority.fence_exclusive(&tx).await?;
        fixture.authority.publish_in(&tx, &mut failed, &prepared).await?;
        let mut publication = store.lock::<Value>(&tx, Kind::Authority, &format!("configuration/{}", failed.value.config_generation)).await?;
        store.save(&tx, &mut publication, "retained", None, Store::now(&tx).await?).await?;
        let failed = failed.value;
        tx.commit().await?;
        for index in [a, b] {
            let address = fixture.child(index)?.address;
            tokio::time::timeout(Duration::from_secs(4), async {
                loop {
                    ensure!(fixture.child(index)?.child.try_wait()?.is_none(), "replica exited during readiness loss");
                    if http.get(format!("http://{address}/health")).send().await?.status() == StatusCode::SERVICE_UNAVAILABLE {
                        return Ok::<(), anyhow::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }).await??;
        }
        ensure!(client_b.revision().await?.config_generation == failed.config_generation);
        ensure!(matches!(client_a.operation("..").await?, OperationLookup::Committed {receipt:other} if *other == receipt));
        fixture.authority.publish(failed.config_generation, &configuration).await?;
        fixture.child(a)?.ready().await?;
        fixture.child(b)?.ready().await?;
        for index in [a, b] { ensure!(fixture.child(index)?.stop().await?.success()); }
        for address in [a_operator, b_operator] { let _listener = tokio::net::TcpListener::bind(address).await?; }
        ensure!(fixture.owners().await?.iter().all(|owner| owner["accepting"] == false));
        fixture.no_local_files()?;
        Ok(())
    }.await;
    fixture.finish(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_operator_auth_and_bind_failures_release_both_listeners() -> Result<()> {
    let mut fixture = Fixture::new(true).await?;
    let result = async {
        let bad = fixture
            .spawn_with_operator(
                "serve",
                available_address()?,
                &[],
                Some((available_address()?, "invalid-secret")),
            )
            .await?;
        ensure!(!fixture.child(bad)?.exited().await?.success());
        let logs = fixture.child(bad)?.logs();
        ensure!(logs.contains("operator token must contain") && !logs.contains("invalid-secret"));
        ensure!(
            fixture.owners().await?.is_empty(),
            "invalid token joined shared authority"
        );

        let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let first = fixture
            .spawn_with_operator(
                "serve",
                available_address()?,
                &[],
                Some((occupied.local_addr()?, TOKEN)),
            )
            .await?;
        ensure!(!fixture.child(first)?.exited().await?.success());
        ensure!(
            fixture
                .child(first)?
                .logs()
                .contains("binding shared operator listener")
        );
        let operator_address = available_address()?;
        let second = fixture
            .spawn_with_operator(
                "serve",
                occupied.local_addr()?,
                &[],
                Some((operator_address, TOKEN)),
            )
            .await?;
        ensure!(!fixture.child(second)?.exited().await?.success());
        ensure!(
            fixture
                .child(second)?
                .logs()
                .contains("binding shared gateway listener")
        );
        let _rebound = tokio::net::TcpListener::bind(operator_address).await?;
        let owners = fixture.owners().await?;
        ensure!(owners.len() == 2 && owners.iter().all(|owner| owner["accepting"] == false));
        fixture.no_local_files()?;
        Ok(())
    }
    .await;
    fixture.finish(result).await
}
