//! Bounded stock-process candidates for part of P1-A07, not load acceptance.
//!
//! Reuse the stock seed, counted HTTP provider, ordinary clients and strict
//! owned-child cleanup. No new provider implementation or production hook.

use std::io::ErrorKind;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::*;

const REQUEST_BYTES: usize = 1024;
const RESPONSE_BYTES: usize = 64 * 1024;
const EXCESS: usize = 4;

#[derive(Clone, Copy)]
enum Case {
    Connections,
    KnownLength,
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_slow_ingress_rejects_excess_connections_without_blocking_replica() -> Result<()> {
    run(Case::Connections).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_configured_body_budget_rejects_known_length_before_provider() -> Result<()> {
    run(Case::KnownLength).await
}

async fn run(case: Case) -> Result<()> {
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
    let mut excess = JoinSet::new();
    // Owners remain outside the timed campaign. Dropping the campaign closes
    // its raw TcpStreams; spawned clients are cancelled and joined below.
    let result = tokio::time::timeout(Duration::from_secs(45), async {
        let world = start_world(&mut fixture, &mock.base, &mut expected).await?;
        match case {
            Case::Connections => connections(&world, &mut mock, &mut requests, &mut excess).await?,
            Case::KnownLength => known_length(&world, &mut mock, &mut requests).await?,
        }
        ensure!(
            requests.is_empty() && excess.is_empty(),
            "unjoined client evidence"
        );
        fixture.no_local_files()?;
        Ok::<usize, anyhow::Error>(match case {
            Case::Connections => 3,
            Case::KnownLength => 2,
        })
    })
    .await
    .context("stock overload campaign deadline exceeded")
    .and_then(|result| result);
    let expected_calls = result.as_ref().ok().copied();
    let mut cleanup = Vec::new();
    requests.abort_all();
    while let Some(joined) = requests.join_next().await {
        match joined {
            Ok(Ok((label, _))) => cleanup.push(Err(anyhow::anyhow!("unconsumed client {label}"))),
            Ok(Err(error)) => cleanup.push(Err(error)),
            Err(error) if error.is_cancelled() => {}
            Err(error) => cleanup.push(Err(error.into())),
        }
    }
    excess.abort_all();
    while let Some(joined) = excess.join_next().await {
        match joined {
            Ok(result) => cleanup.push(result),
            Err(error) if error.is_cancelled() => {}
            Err(error) => cleanup.push(Err(error.into())),
        }
    }
    let result = combine_results(result.map(|_| ()), cleanup);
    // Close the counted observation window only after both gateways have
    // stopped. The provider stays alive during their strict, clean shutdown.
    let result = finish_children(
        &mut fixture.children,
        result,
        &expected,
        &failure_log_prefix(),
    )
    .await;
    let provider_joined = mock.finish().await;
    let count_checked = if let Some(expected) = expected_calls {
        let actual = calls.load(Ordering::SeqCst);
        if actual == expected {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "observed {actual} provider HTTP entries, expected {expected}"
            ))
        }
    } else {
        Ok(())
    };
    let result = combine_results(result, vec![provider_joined, count_checked]);
    // Children are already reaped; this rechecks their cached statuses and
    // retains final logs through pool closure and owned database teardown.
    finish_empty_fixture_with_database(database, fixture, result, &expected).await
}

async fn start_world(
    fixture: &mut Fixture,
    base: &str,
    expected: &mut ExitExpectations,
) -> Result<World> {
    let keys = std::array::from_fn(|_| crate::auth::keys::generate());
    seed(fixture, &seed_plan(base, &keys)).await?;
    let mut replicas = Vec::new();
    let mut children = Vec::new();
    let mut operators = Vec::new();
    for replica in 0..2 {
        let inference = available_address()?;
        let operator = (0..8)
            .map(|_| available_address())
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .find(|address| *address != inference)
            .context("distinct operator address unavailable")?;
        let child = if replica == 0 {
            spawn_bounded(fixture, inference, operator).await?
        } else {
            fixture
                .spawn_with_operator("serve", inference, &[], Some((operator, OPERATOR_TOKEN)))
                .await?
        };
        expected.insert(child, ExpectedExit::CleanServe);
        fixture.child(child)?.ready().await?;
        replicas.push(inference);
        children.push(child);
        operators.push(OperatorClient::new(
            &format!("http://{operator}"),
            OPERATOR_TOKEN,
        )?);
    }
    Ok(World {
        http: reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(8))
            .build()?,
        replicas: replicas
            .try_into()
            .map_err(|_| anyhow::anyhow!("replica pair missing"))?,
        children: children
            .try_into()
            .map_err(|_| anyhow::anyhow!("child pair missing"))?,
        operators: operators
            .try_into()
            .map_err(|_| anyhow::anyhow!("operator pair missing"))?,
        keys,
    })
}

// Fixture::spawn_with_operator has fixed process bounds. Keep this one local
// override in the child instead of editing the parent's moving launcher.
async fn spawn_bounded(
    fixture: &mut Fixture,
    address: SocketAddr,
    operator: SocketAddr,
) -> Result<usize> {
    let index = fixture.children.len();
    let config = fixture.root.path().join(format!("gateway-{index}.json"));
    let document = json!({
        "server":{"listen":address.to_string(),"log_level":"info"},
        "database":{"url":"${FIXTURE_SHARED_DATABASE}"},
        "gateway_state":{"mode":"shared","namespace":fixture.authority.store().namespace(),
            "deployment_key_env":"FIXTURE_SHARED_KEY","pool_connections":3,
            "transaction_timeout_ms":1000,"owner_lease_ms":10000,"heartbeat_ms":250,
            "worker_interval_ms":100,"drain_timeout_ms":2000,
            "max_connections":1,"local_max_in_flight":4,"max_request_bytes":REQUEST_BYTES,
            "header_timeout_ms":2000,"request_timeout_ms":20000,"write_stall_timeout_ms":2000},
        "operator":{"listen":operator.to_string(),"credentials":[{
            "id":"fixture-operator","token_env":"FIXTURE_OPERATOR_TOKEN","grants":["read","policy_write"]}]}
    });
    tokio::fs::write(&config, serde_json::to_vec(&document)?).await?;
    let log = fixture.root.path().join(format!("gateway-{index}.log"));
    let output = std::fs::File::create(&log)?;
    let child = Command::new(&fixture.binary)
        .arg("serve")
        .arg("--config")
        .arg(&config)
        .current_dir(fixture.root.path().join("launch"))
        .env("FIXTURE_SHARED_DATABASE", &fixture.url)
        .env("FIXTURE_SHARED_KEY", STANDARD.encode(fixture.key))
        .env("FIXTURE_OPERATOR_TOKEN", OPERATOR_TOKEN)
        .env(
            "BITROUTER_HOME",
            fixture.root.path().join("unexpected-home"),
        )
        .env("HOME", fixture.root.path().join("unexpected-user-home"))
        .env_remove("RUST_LOG")
        .env_remove("BITROUTER_WORKFLOW_TRACE_JSONL")
        .stdin(Stdio::null())
        .stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(output))
        .kill_on_drop(true)
        .spawn()?;
    fixture.children.push(Running {
        child,
        log,
        address,
    });
    Ok(index)
}

fn payload(tag: &str) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(
        &json!({"model":"tenant:mock","messages":[{"role":"user","content":tag}]}),
    )?;
    ensure!(body.len() < REQUEST_BYTES);
    Ok(body)
}

fn head(w: &World, tag: &str, length: usize, expect: bool) -> String {
    let expectation = if expect {
        "Expect: 100-continue\r\n"
    } else {
        ""
    };
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nIdempotency-Key: {tag}\r\nx-bitrouter-request-id: {tag}\r\nConnection: close\r\n{expectation}\r\n",
        w.replicas[0], w.keys[0].secret
    )
}

async fn connections(
    w: &World,
    mock: &mut Mock,
    requests: &mut Requests,
    excess: &mut JoinSet<Result<()>>,
) -> Result<()> {
    let tag = "slow-body";
    let body = payload(tag)?;
    let (mut held, interim) = open_head(w, tag, body.len(), true).await?;
    interim.expect_status(StatusCode::CONTINUE)?;
    // Hyper 1.10.1 writes this interim response when its body is polled, not
    // merely when TCP connects. Leave a valid body's final bytes unsent.
    let split = body.len() / 2;
    held.write_all(&body[..split]).await?;
    reject_burst(w, excess, "before-progress").await?;
    ensure!(mock.calls() == 0);
    survivor_progress(w, mock, requests, "unaffected").await?;
    reject_burst(w, excess, "after-progress").await?;
    ensure!(mock.calls() == 1);

    // Completing this SAME body and observing its provider request proves it
    // stayed live throughout both rejection bursts and survivor progress. A
    // body deadline/closed socket cannot accidentally pass the holding phase.
    held.write_all(&body[split..]).await?;
    let arrival = mock.arrival(tag, "tenant", SECRETS[0]).await?;
    let id = arrival.execution.clone();
    arrival.release(Reply::Success)?;
    let wire = tokio::time::timeout(Duration::from_secs(4), read_final(&mut held))
        .await
        .context("released slow-body client did not receive a complete response")??;
    success(&wire, &id, tag)?;
    drop(held);
    w.wait_state(1, 0, &id, succeeded).await?;
    ensure!(mock.calls() == 2);
    owner_progress(w, mock, "after-release").await?;
    ensure!(mock.calls() == 3);
    Ok(())
}

async fn reject_burst(w: &World, tasks: &mut JoinSet<Result<()>>, phase: &str) -> Result<()> {
    for index in 0..EXCESS {
        let tag = format!("excess-{phase}-{index}");
        let body = payload(&tag)?;
        let mut request = head(w, &tag, body.len(), false).into_bytes();
        request.extend_from_slice(&body);
        let address = w.replicas[0];
        tasks.spawn(async move {
            // Success means a connected excess peer observed closure/reset,
            // never connect failure, an HTTP response, or elapsed silence.
            tokio::time::timeout(Duration::from_secs(1), async {
                let mut socket = TcpStream::connect(address).await?;
                match socket.write_all(&request).await {
                    Err(error) if peer_reset(&error) => return Ok(()),
                    other => other?,
                }
                let mut byte = [0_u8; 1];
                match socket.read(&mut byte).await {
                    Ok(0) => Ok(()),
                    Err(error) if peer_reset(&error) => Ok(()),
                    Ok(_) => anyhow::bail!("excess connection entered HTTP service"),
                    Err(error) => Err(error.into()),
                }
            })
            .await
            .with_context(|| format!("excess {tag} queued beyond the 1s rejection budget"))?
        });
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut joined = 0;
        while let Some(result) = tasks.join_next().await {
            result??;
            joined += 1;
        }
        ensure!(joined == EXCESS);
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("excess clients did not all join")??;
    Ok(())
}

fn peer_reset(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        ErrorKind::ConnectionReset | ErrorKind::BrokenPipe | ErrorKind::ConnectionAborted
    )
}

async fn known_length(w: &World, mock: &mut Mock, requests: &mut Requests) -> Result<()> {
    // Advertise one byte beyond the configured cap, without sending a body.
    // An early 413 positively demonstrates the stock config reached transport.
    let (mut client, response) = open_head(w, "oversized", REQUEST_BYTES + 1, false).await?;
    response.expect_status(StatusCode::PAYLOAD_TOO_LARGE)?;
    let response = tokio::time::timeout(Duration::from_secs(2), read_body(&mut client, response))
        .await
        .context("oversized response did not finish")??;
    response.expect_status(StatusCode::PAYLOAD_TOO_LARGE)?;
    drop(client);
    ensure!(mock.calls() == 0);
    survivor_progress(w, mock, requests, "body-budget-survivor").await?;
    owner_progress(w, mock, "body-budget-released").await?;
    ensure!(mock.calls() == 2);
    Ok(())
}

async fn survivor_progress(
    w: &World,
    mock: &mut Mock,
    requests: &mut Requests,
    tag: &str,
) -> Result<()> {
    let before = mock.calls();
    start(requests, tag, w.post(1, 3, "other:mock", tag, tag, tag));
    let arrival = mock.arrival(tag, "other", SECRETS[2]).await?;
    let id = arrival.execution.clone();
    arrival.release(Reply::Success)?;
    success(&completed(requests, tag).await?, &id, tag)?;
    w.wait_state(1, 3, &id, succeeded).await?;
    ensure!(
        mock.calls() == before + 1,
        "progress request was duplicated"
    );
    Ok(())
}

async fn owner_progress(w: &World, mock: &mut Mock, tag: &str) -> Result<()> {
    let before = mock.calls();
    let body = payload(tag)?;
    let (mut client, interim) = open_head(w, tag, body.len(), true).await?;
    interim.expect_status(StatusCode::CONTINUE)?;
    client.write_all(&body).await?;
    let arrival = mock.arrival(tag, "tenant", SECRETS[0]).await?;
    let id = arrival.execution.clone();
    arrival.release(Reply::Success)?;
    let wire = tokio::time::timeout(Duration::from_secs(4), read_final(&mut client))
        .await
        .context("post-release owner response did not finish")??;
    success(&wire, &id, tag)?;
    drop(client);
    w.wait_state(1, 0, &id, succeeded).await?;
    ensure!(mock.calls() == before + 1, "owner progress was duplicated");
    Ok(())
}

// A completed health/previous response can leave a task awaiting reaping at
// max_connections=1. Retry setup only after observed close/reset and BEFORE
// sending any body; no inference can have been dispatched by these retries.
// The excess probes never use this helper and are never retried.
async fn open_head(w: &World, tag: &str, length: usize, expect: bool) -> Result<(TcpStream, Wire)> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let mut socket = TcpStream::connect(w.replicas[0]).await?;
            let response = tokio::time::timeout(Duration::from_secs(2), async {
                socket
                    .write_all(head(w, tag, length, expect).as_bytes())
                    .await?;
                read_head(&mut socket).await
            })
            .await
            .context("stock transport did not answer request headers within 2s")?;
            match response {
                Ok(wire) => return Ok((socket, wire)),
                Err(error)
                    if error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                        peer_reset(error) || error.kind() == ErrorKind::UnexpectedEof
                    }) =>
                {
                    drop(socket);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .with_context(|| format!("header-only setup for {tag} never admitted within 3s"))?
}

fn success(wire: &Wire, id: &str, tag: &str) -> Result<()> {
    wire.expect_status(StatusCode::OK)?;
    wire.identity(id, tag)?;
    let body: Value = serde_json::from_slice(&wire.body)?;
    ensure!(body["choices"][0]["finish_reason"] == "stop");
    ensure!(body["choices"][0]["message"]["content"] == "ok");
    Ok(())
}

// Tiny bounded HTTP/1 fixture reader for replica 0; survivor inference uses
// the existing reqwest client. Read the head one byte at a time so a 100
// Continue never consumes bytes from a later response.
async fn read_head(socket: &mut TcpStream) -> Result<Wire> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        ensure!(
            bytes.len() < 8192,
            "raw response header exceeded fixture bound"
        );
        bytes.push(socket.read_u8().await?);
    }
    parse_head(&bytes)
}

fn parse_head(bytes: &[u8]) -> Result<Wire> {
    let mut lines = std::str::from_utf8(bytes)?.split("\r\n");
    let mut status = lines
        .next()
        .context("HTTP status missing")?
        .split_whitespace();
    ensure!(status.next() == Some("HTTP/1.1"));
    let status = StatusCode::from_u16(status.next().context("HTTP status code missing")?.parse()?)?;
    let mut headers = reqwest::header::HeaderMap::new();
    for line in lines.take_while(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .context("invalid HTTP response header")?;
        headers.append(
            reqwest::header::HeaderName::from_bytes(name.as_bytes())?,
            value.trim().parse()?,
        );
    }
    Ok(Wire {
        status,
        headers,
        body: Vec::new(),
    })
}

async fn read_final(socket: &mut TcpStream) -> Result<Wire> {
    let wire = read_head(socket).await?;
    read_body(socket, wire).await
}

async fn read_body(socket: &mut TcpStream, mut wire: Wire) -> Result<Wire> {
    ensure!(
        !wire.status.is_informational(),
        "unexpected interim response"
    );
    let mut encoded = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let count = socket.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        ensure!(
            encoded.len().saturating_add(count) <= RESPONSE_BYTES,
            "raw response exceeded fixture bound"
        );
        encoded.extend_from_slice(&buffer[..count]);
    }
    wire.body = decode_body(&wire, &encoded)?;
    Ok(wire)
}

fn decode_body(wire: &Wire, encoded: &[u8]) -> Result<Vec<u8>> {
    ensure!(encoded.len() <= RESPONSE_BYTES);
    if wire.headers.contains_key("transfer-encoding") {
        ensure!(!wire.headers.contains_key("content-length"));
        ensure!(
            wire.header("transfer-encoding")?
                .eq_ignore_ascii_case("chunked")
        );
        let mut remaining = encoded;
        let mut body = Vec::new();
        loop {
            let end = remaining
                .windows(2)
                .position(|pair| pair == b"\r\n")
                .context("truncated chunk size")?;
            let size = std::str::from_utf8(&remaining[..end])?
                .split(';')
                .next()
                .context("chunk size missing")?;
            let size = usize::from_str_radix(size, 16)?;
            remaining = &remaining[end + 2..];
            if size == 0 {
                ensure!(
                    remaining == b"\r\n",
                    "fixture response has trailers or missing chunk terminator"
                );
                return Ok(body);
            }
            let framed = size.checked_add(2).context("chunk length overflow")?;
            ensure!(
                framed <= remaining.len()
                    && remaining.get(size..framed) == Some(b"\r\n".as_slice()),
                "truncated response chunk"
            );
            body.extend_from_slice(&remaining[..size]);
            remaining = &remaining[framed..];
        }
    }
    if wire.headers.contains_key("content-length") {
        let length: usize = wire.header("content-length")?.parse()?;
        ensure!(length == encoded.len(), "truncated or extra response bytes");
    }
    // Without length or transfer coding, the observed EOF delimits HTTP/1.1.
    Ok(encoded.to_vec())
}

#[test]
fn raw_client_rejects_truncated_or_ambiguous_success_framing() -> Result<()> {
    let mut wire = parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n")?;
    ensure!(decode_body(&wire, b"{}").is_ok());
    ensure!(decode_body(&wire, b"{").is_err());
    ensure!(decode_body(&wire, b"{}x").is_err());
    wire.headers.remove("content-length");
    wire.headers.insert("transfer-encoding", "chunked".parse()?);
    ensure!(decode_body(&wire, b"2\r\n{}\r\n0\r\n\r\n")? == b"{}");
    ensure!(decode_body(&wire, b"2\r\n{}\r\n").is_err());
    wire.headers.insert("content-length", "2".parse()?);
    ensure!(decode_body(&wire, b"2\r\n{}\r\n0\r\n\r\n").is_err());
    let duplicate =
        parse_head(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nContent-Length: 2\r\n\r\n")?;
    ensure!(decode_body(&duplicate, b"{}").is_err());
    Ok(())
}
