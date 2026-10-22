//! Concurrent requests share one tenant, principal, resource and static key.
//! The stock binary owns all state transitions; the provider barrier proves
//! overlap and counts every dispatch through gateway shutdown.

use std::collections::{BTreeMap, BTreeSet};

use super::*;

#[path = "concurrent_static/latency.rs"]
mod latency;

const CONCURRENCY: usize = 16;
const WAVES: usize = 3;

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_concurrent_static_http1_single_instance() -> Result<()> {
    run(1, reqwest::Version::HTTP_11).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_concurrent_static_http1_two_instances() -> Result<()> {
    run(2, reqwest::Version::HTTP_11).await
}

#[cfg(feature = "http2")]
#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and an HTTP/2 BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_concurrent_static_http2_single_instance() -> Result<()> {
    run(1, reqwest::Version::HTTP_2).await
}

#[cfg(feature = "http2")]
#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and an HTTP/2 BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_concurrent_static_http2_two_instances() -> Result<()> {
    run(2, reqwest::Version::HTTP_2).await
}

async fn run(instances: usize, version: reqwest::Version) -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let mut fixture = match empty_fixture(&mut database).await {
        Ok(fixture) => fixture,
        Err(error) => return database.finish(Err(error)).await,
    };
    let mut expected = ExitExpectations::new();
    let mut mock = match Mock::start().await {
        Ok(mock) => mock,
        Err(error) => {
            return finish_empty_fixture_with_database(database, fixture, Err(error), &expected)
                .await;
        }
    };
    let calls = mock.state.calls.clone();
    let mut requests = Requests::new();
    let relay = match latency::Relay::start(&fixture.url).await {
        Ok(relay) => relay,
        Err(error) => {
            let result = combine_results(Err(error), vec![mock.finish().await]);
            return finish_empty_fixture_with_database(database, fixture, result, &expected).await;
        }
    };
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let keys = std::array::from_fn(|_| crate::auth::keys::generate());
        let mut plan = seed_plan(&mock.base, &keys);
        for resource in plan["resources"].as_array_mut().context("missing resources")? {
            resource["concurrent_attempts"] = json!(CONCURRENCY + 1);
        }
        for tenant in plan["tenants"].as_array_mut().context("missing tenants")? {
            tenant["limits"]["concurrent_executions"] = json!(CONCURRENCY);
        }
        seed(&mut fixture, &plan).await?;
        // Bootstrap and the fixture's administrative pool stay direct. Only
        // stock serving processes use the delayed, owned database connection.
        fixture.url.clone_from(&relay.url);
        let mut replicas = Vec::new();
        for _ in 0..instances {
            let address = available_address()?;
            // Match the documented shared defaults: 16 connections (including
            // the heartbeat reservation) and a total 3-second transaction bound.
            let child = fixture.spawn_with_pool("serve", address, &[], None, (16, 3000)).await?;
            expected.insert(child, ExpectedExit::CleanServe);
            fixture.child(child)?.ready().await?;
            replicas.push(address);
        }
        let builder = reqwest::Client::builder().no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never()).timeout(Duration::from_secs(12));
        let builder = if version == reqwest::Version::HTTP_11 { builder.http1_only() } else { builder };
        #[cfg(feature = "http2")]
        let builder = if version == reqwest::Version::HTTP_2 { builder.http2_prior_knowledge() } else { builder };
        let http = builder.build()?;
        for wave in 0..WAVES {
            let mut wanted = BTreeSet::new();
            for index in 0..CONCURRENCY {
                let tag = format!("wave-{wave}-request-{index}");
                wanted.insert(tag.clone());
                let request = post(&http, replicas[index % instances], &keys[0].secret, &tag);
                requests.spawn(async move {
                    let response = request.send().await?;
                    ensure!(response.version() == version, "unexpected inference HTTP version");
                    Ok((tag, Wire::read(response).await?))
                });
            }
            let mut arrivals = Vec::new();
            let mut identities = BTreeMap::new();
            while arrivals.len() < CONCURRENCY {
                tokio::select! {
                    arrival = mock.arrivals.recv() => {
                        let arrival = arrival.context("provider arrivals ended")?;
                        ensure!(wanted.remove(&arrival.tag), "unexpected or duplicate provider dispatch");
                        ensure!(arrival.authorization == format!("Bearer {}", SECRETS[0]));
                        ensure!(arrival.path == "/tenant/chat/completions" && arrival.model == "mock");
                        ensure!(uuid::Uuid::parse_str(&arrival.execution).is_ok());
                        identities.insert(arrival.tag.clone(), arrival.execution.clone());
                        arrivals.push(arrival);
                    }
                    result = requests.join_next() => {
                        let (tag, wire) = result.context("missing client")???;
                        wire.expect_status(StatusCode::OK).with_context(|| format!("{tag} failed before provider barrier"))?;
                        anyhow::bail!("{tag} completed before provider release");
                    }
                }
            }
            ensure!(identities.values().collect::<BTreeSet<_>>().len() == CONCURRENCY);
            // A full wave must retain exactly the tenant capacity across all
            // replicas, and the rejected request must never reach the provider.
            let denied = Wire::read(post(&http, replicas[wave % instances], &keys[0].secret, &format!("denied-{wave}")).send().await?).await?;
            denied.expect_status(StatusCode::TOO_MANY_REQUESTS)?;
            ensure!(!denied.headers.contains_key("x-bitrouter-execution-id") && !denied.headers.contains_key("location"), "tenant overflow admitted an execution");
            for arrival in arrivals { arrival.release(Reply::Success)?; }
            while !requests.is_empty() {
                let (tag, wire) = completed_any(&mut requests).await?;
                wire.expect_status(StatusCode::OK)?;
                wire.identity(identities.get(&tag).context("unknown completion")?, &tag)?;
                let body: Value = serde_json::from_slice(&wire.body)?;
                ensure!(body["choices"][0]["message"]["content"] == "ok");
            }
            // Observe finalization on the other replica before the next full
            // wave. Capacity release must not depend on a process-local cache.
            for (index, (tag, id)) in identities.iter().enumerate() {
                tokio::time::timeout(Duration::from_secs(4), async {
                    loop {
                        let wire = Wire::read(http.get(format!("http://{}/v1/gateway/executions/{id}", replicas[(index + 1) % instances])).bearer_auth(&keys[0].secret).send().await?).await?;
                        wire.expect_status(StatusCode::OK)?;
                        wire.identity(id, tag)?;
                        let view = wire.execution()?;
                        ensure!(view.attempts.len() == 1);
                        if succeeded(&view.state) { return Ok::<(), anyhow::Error>(()); }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }).await.context("execution did not finalize")??;
                let duplicate = Wire::read(post(&http, replicas[(index + 1) % instances], &keys[0].secret, tag).send().await?).await?;
                duplicate.expect_status(StatusCode::ACCEPTED)?;
                duplicate.identity(id, tag)?;
            }
            ensure!(mock.calls() == (wave + 1) * CONCURRENCY, "provider replay or excess dispatch");
        }
        // Terminal HTTP state precedes asynchronous metering/projection delivery.
        // Observe that work settling through the owned fixture's read-only pool
        // before requesting the fixture's short, two-second shutdown. Serving
        // processes keep the delayed database path throughout this observation.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let pending = fixture.authority.store()
                    .due::<Value>(Kind::Outbox, "pending", i64::MAX, 1).await?;
                if pending.is_empty() { return Ok::<(), anyhow::Error>(()); }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        }).await.context("terminal outbox did not settle")??;
        fixture.no_local_files()?;
        Ok(())
    }).await.context("concurrent static process campaign deadline").and_then(|result| result);
    requests.abort_all();
    let mut cleanup = Vec::new();
    while let Some(joined) = requests.join_next().await {
        match joined {
            Ok(Ok((tag, _))) => cleanup.push(Err(anyhow::anyhow!("unconsumed client {tag}"))),
            Ok(Err(error)) => cleanup.push(Err(error)),
            Err(error) if error.is_cancelled() => {}
            Err(error) => cleanup.push(Err(error.into())),
        }
    }
    let succeeded = result.is_ok();
    let result = finish_children(
        &mut fixture.children,
        combine_results(result, cleanup),
        &expected,
        &failure_log_prefix(),
    )
    .await;
    let joined = mock.finish().await;
    let relay_joined = relay.finish().await;
    let counted = if succeeded && calls.load(Ordering::SeqCst) != WAVES * CONCURRENCY {
        Err(anyhow::anyhow!(
            "provider dispatch count changed during gateway shutdown"
        ))
    } else {
        Ok(())
    };
    finish_empty_fixture_with_database(
        database,
        fixture,
        combine_results(result, vec![joined, counted, relay_joined]),
        &expected,
    )
    .await
}

fn post(
    http: &reqwest::Client,
    address: SocketAddr,
    key: &str,
    tag: &str,
) -> reqwest::RequestBuilder {
    http.post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(key)
        .header("idempotency-key", tag)
        .header("x-bitrouter-request-id", tag)
        .json(&json!({"model":"tenant:mock","messages":[{"role":"user","content":tag}]}))
}
