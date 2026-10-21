//! Stock processes lose only their fixture-owned PostgreSQL TCP path.
//!
//! No database role, server, container, user process or backend PID is changed.

use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::*;

#[tokio::test]
#[ignore = "requires explicit TCP PostgreSQL host/port, CREATEDB and BITROUTER_TEST_GATEWAY_BINARY"]
async fn official_loaded_replicas_fail_closed_and_recover_after_database_connection_loss()
-> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let mut fixture = match empty_fixture(&mut database).await {
        Ok(fixture) => fixture,
        Err(error) => return database.finish(Err(error)).await,
    };
    let mut expected = ExitExpectations::new();
    let result = run_proxy(&mut fixture, &mut expected).await;
    // run_proxy joins children, upstream and proxy before the registered direct
    // pool is closed and the UUID-owned database is dropped. Keep logs alive.
    let result = database.finish(result).await;
    retain_failed_logs(
        fixture.children.iter().map(|child| child.log.as_path()),
        result,
        &failure_log_prefix(),
    )
}

async fn run_proxy(fixture: &mut Fixture, expected: &mut ExitExpectations) -> Result<()> {
    let mut proxy = Proxy::start(&fixture.url).await?;
    // The in-process registered fixture pool remains direct. Only the stock
    // bootstrap and serving children receive the proxy URL.
    fixture.url.clone_from(&proxy.url);
    let mut mock = match Mock::start().await {
        Ok(mock) => mock,
        Err(error) => return combine_results(Err(error), vec![proxy.finish().await]),
    };
    let calls = mock.state.calls.clone();
    let mut jobs = Requests::new();
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let world = start_world(fixture, &mock.base, expected).await?;
        let mut originals = Vec::new();
        for replica in 0..2 {
            originals.push(success(&world, &mut mock, &mut jobs, replica, "before").await?);
        }
        let before = proxy.command(Control::Inspect).await?;
        ensure!(
            before.forwarded > 0 && before.active > 0,
            "no live database relays to cut"
        );
        let cut = proxy.command(Control::Cut).await?;
        ensure!(cut.dropped > 0 && cut.active == 0 && !cut.open);
        let fault = tokio::time::timeout(Duration::from_secs(12), async {
            health(&world, StatusCode::SERVICE_UNAVAILABLE).await?;
            for round in 0..2 {
                // Status authenticates against durable state independently of
                // readiness. Neither it nor fresh inference may use cached auth.
                let probes = (0..2).map(|replica| {
                    let world = &world;
                    let original = &originals[replica];
                    async move {
                        let label = format!("database-cut-{round}-{replica}");
                        let denied = Wire::read(
                            world
                                .post(replica, 0, "tenant:mock", &label, &label, &label)
                                .send()
                                .await?,
                        )
                        .await?;
                        denied.expect_status(StatusCode::SERVICE_UNAVAILABLE)?;
                        world
                            .status(replica, 0, &original.execution_id)
                            .await?
                            .expect_status(StatusCode::SERVICE_UNAVAILABLE)?;
                        Ok::<(), anyhow::Error>(())
                    }
                });
                for probe in futures::future::join_all(probes).await {
                    probe?;
                }
                ensure!(
                    mock.calls() == 2,
                    "backend loss dispatched another HTTP request"
                );
            }
            let closed = proxy.command(Control::Inspect).await?;
            ensure!(
                !closed.open && closed.active == 0 && closed.blocked > cut.blocked,
                "gateway did not retry its closed database path"
            );
            for child in world.children {
                ensure!(
                    fixture.child(child)?.child.try_wait()?.is_none(),
                    "host exited during short outage"
                );
            }
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("outage exceeded its 12s budget within the 60s owner lease")
        .and_then(|result| result);
        // Restore even if an outage assertion failed. Cleanup repeats this
        // idempotently if this future is cancelled before the acknowledgement.
        let restored = proxy.command(Control::Open).await.map(|_| ());
        combine_results(fault, vec![restored])?;
        health(&world, StatusCode::OK).await?;
        for replica in 0..2 {
            for original in &originals {
                let wire = world.status(replica, 0, &original.execution_id).await?;
                wire.expect_status(StatusCode::OK)?;
                same_facts(&wire.execution()?, original)?;
            }
            success(&world, &mut mock, &mut jobs, replica, "after").await?;
            ensure!(
                fixture
                    .child(world.children[replica])?
                    .child
                    .try_wait()?
                    .is_none()
            );
        }
        ensure!(mock.calls() == 4 && jobs.is_empty());
        fixture.no_local_files()
    })
    .await
    .context("backend-loss process campaign deadline exceeded")
    .and_then(|result| result);
    let completed_campaign = result.is_ok();
    let joined = join_clients(&mut jobs).await;
    let restored = proxy.command(Control::Open).await.map(|_| ());
    let result = combine_results(result, vec![joined, restored]);
    // Leave DB forwarding and upstream available until every owned child has
    // actually exited. Then join both fixture servers before DROP DATABASE.
    let result = finish_children(
        &mut fixture.children,
        result,
        expected,
        &failure_log_prefix(),
    )
    .await;
    let mock_joined = mock.finish().await;
    let proxy_joined = proxy.finish().await;
    let count = if !completed_campaign || calls.load(Ordering::SeqCst) == 4 {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "late upstream dispatch after outage campaign: {}",
            calls.load(Ordering::SeqCst)
        ))
    };
    combine_results(result, vec![mock_joined, proxy_joined, count])
}

async fn join_clients(jobs: &mut Requests) -> Result<()> {
    jobs.abort_all();
    let mut results = Vec::new();
    while let Some(result) = jobs.join_next().await {
        results.push(match result {
            Ok(Ok((label, _))) => Err(anyhow::anyhow!("unconsumed fixture client {label}")),
            Ok(Err(error)) => Err(error),
            Err(error) if error.is_cancelled() => Ok(()),
            Err(error) => Err(error.into()),
        });
    }
    combine_results(Ok(()), results)
}

async fn success(
    w: &World,
    mock: &mut Mock,
    jobs: &mut Requests,
    replica: usize,
    phase: &str,
) -> Result<GatewayExecution> {
    let label = format!("backend-{phase}-{replica}");
    let before = mock.calls();
    start(
        jobs,
        &label,
        w.post(replica, 0, "tenant:mock", &label, &label, &label),
    );
    let arrival = mock.arrival(&label, "tenant", SECRETS[0]).await?;
    let id = arrival.execution.clone();
    arrival.release(Reply::Success)?;
    let wire = completed(jobs, &label).await?;
    wire.expect_status(StatusCode::OK)?;
    wire.identity(&id, &label)?;
    let view = w.wait_state(1 - replica, 0, &id, succeeded).await?;
    ensure!(view.attempts.len() == 1 && mock.calls() == before + 1);
    Ok(view)
}

async fn health(world: &World, wanted: StatusCode) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let mut all = true;
            for replica in world.replicas {
                let status = world
                    .http
                    .get(format!("http://{replica}/health"))
                    .send()
                    .await?
                    .status();
                ensure!(matches!(
                    status,
                    StatusCode::OK | StatusCode::SERVICE_UNAVAILABLE
                ));
                all &= status == wanted;
            }
            if all {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .context("same hosts did not converge on expected readiness")?
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
    for _ in 0..2 {
        let inference = available_address()?;
        let operator = loop {
            let address = available_address()?;
            if address != inference {
                break address;
            }
        };
        let child = spawn(fixture, inference, operator).await?;
        expected.insert(child, ExpectedExit::CleanServe);
        children.push(child);
        fixture.child(child)?.ready().await?;
        replicas.push(inference);
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
            .timeout(Duration::from_secs(5))
            .build()?,
        replicas: replicas
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing replicas"))?,
        children: children
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing children"))?,
        operators: operators
            .try_into()
            .map_err(|_| anyhow::anyhow!("missing operators"))?,
        keys,
    })
}

async fn spawn(fixture: &mut Fixture, address: SocketAddr, operator: SocketAddr) -> Result<usize> {
    let index = fixture.children.len();
    let config = fixture.root.path().join(format!("gateway-{index}.json"));
    tokio::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "server":{"listen":address.to_string(),"log_level":"info"},
            "database":{"url":"${FIXTURE_SHARED_DATABASE}"},
            "gateway_state":{"mode":"shared","namespace":fixture.authority.store().namespace(),
                "deployment_key_env":"FIXTURE_SHARED_KEY","pool_connections":3,
                "transaction_timeout_ms":1000,"owner_lease_ms":60000,"heartbeat_ms":250,
                "worker_interval_ms":100,"drain_timeout_ms":2000},
            "operator":{"listen":operator.to_string(),"credentials":[{
                "id":"fixture-operator","token_env":"FIXTURE_OPERATOR_TOKEN","grants":["read"]}]}
        }))?,
    )
    .await?;
    let log = fixture.root.path().join(format!("gateway-{index}.log"));
    let output = std::fs::File::create(&log)?;
    let child = Command::new(&fixture.binary)
        .args(["serve", "--config"])
        .arg(config)
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

#[derive(Clone, Copy)]
enum Control {
    Inspect,
    Cut,
    Open,
    Stop,
}

#[derive(Clone, Copy, Default)]
struct Stats {
    open: bool,
    active: usize,
    forwarded: usize,
    blocked: usize,
    dropped: usize,
}

struct Proxy {
    url: String,
    control: mpsc::Sender<(Control, oneshot::Sender<Stats>)>,
    root: JoinHandle<Result<()>>,
}

impl Proxy {
    async fn start(source: &str) -> Result<Self> {
        let (mut url, upstream) = tcp_target(source)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        url.set_host(Some("127.0.0.1"))
            .map_err(|_| anyhow::anyhow!("invalid proxy host"))?;
        url.set_port(Some(address.port()))
            .map_err(|_| anyhow::anyhow!("invalid proxy port"))?;
        let (control, commands) = mpsc::channel(4);
        let root = tokio::spawn(relay(listener, upstream, commands));
        Ok(Self {
            url: url.to_string(),
            control,
            root,
        })
    }

    async fn command(&self, action: Control) -> Result<Stats> {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (send, receive) = oneshot::channel();
            self.control
                .send((action, send))
                .await
                .map_err(|_| anyhow::anyhow!("database proxy control ended"))?;
            receive
                .await
                .context("database proxy acknowledgement missing")
        })
        .await
        .context("database proxy control stalled")?
    }

    async fn finish(&mut self) -> Result<()> {
        let stop = self.command(Control::Stop).await.map(|_| ());
        let joined = match tokio::time::timeout(Duration::from_secs(3), &mut self.root).await {
            Ok(result) => result
                .context("database proxy root failed")
                .and_then(|result| result),
            Err(_) => {
                self.root.abort();
                let joined = (&mut self.root).await;
                let extra = match joined {
                    Err(error) if error.is_cancelled() => Ok(()),
                    Err(error) => Err(error.into()),
                    Ok(result) => result,
                };
                combine_results(
                    Err(anyhow::anyhow!("database proxy required forced join")),
                    vec![extra],
                )
            }
        };
        combine_results(stop, vec![joined])
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.root.abort();
    }
}

fn tcp_target(source: &str) -> Result<(url::Url, (String, u16))> {
    let url = url::Url::parse(source).context("invalid fixture database URL")?;
    ensure!(matches!(url.scheme(), "postgres" | "postgresql"));
    let host = url.host_str().context("TCP database host required")?;
    ensure!(
        !host.is_empty() && !host.contains(',') && !host.contains('%') && !host.starts_with('/'),
        "proxy fixture requires one explicit TCP host"
    );
    // Reject decoded routing overrides rather than silently bypassing the
    // proxy or changing the selected database. The owned-DB helper already
    // removes dbname, but this boundary must remain independently strict.
    // https://docs.rs/sqlx-postgres/latest/sqlx_postgres/struct.PgConnectOptions.html
    for (key, value) in url.query_pairs() {
        ensure!(
            !matches!(
                key.as_ref(),
                "host" | "hostaddr" | "port" | "dbname" | "database" | "service"
            ),
            "database URL routing override is unsupported by the TCP proxy fixture"
        );
        if matches!(key.as_ref(), "sslmode" | "ssl-mode") {
            ensure!(
                matches!(value.as_ref(), "disable" | "allow" | "prefer" | "require"),
                "proxy fixture requires TLS without endpoint-name verification"
            );
        }
    }
    let endpoint = (
        host.trim_matches(['[', ']']).to_owned(),
        url.port()
            .context("proxy fixture requires an explicit TCP port")?,
    );
    Ok((url, endpoint))
}

async fn relay(
    listener: TcpListener,
    upstream: (String, u16),
    mut controls: mpsc::Receiver<(Control, oneshot::Sender<Stats>)>,
) -> Result<()> {
    // Relays are inline futures, not spawned children. Clearing this set drops
    // both owned sockets (including pending connect futures) before cut ACK.
    // Joining this single root therefore joins every forwarding computation.
    let mut relays: FuturesUnordered<BoxFuture<'static, std::io::Result<()>>> =
        FuturesUnordered::new();
    let mut stats = Stats {
        open: true,
        ..Stats::default()
    };
    loop {
        tokio::select! {
            biased;
            control = controls.recv() => {
                let Some((action, reply)) = control else { return Ok(()); };
                match action {
                    Control::Inspect => {},
                    Control::Open => stats.open = true,
                    Control::Cut | Control::Stop => {
                        stats.open = false;
                        stats.dropped += relays.len();
                        relays.clear();
                    }
                }
                stats.active = relays.len();
                let _ = reply.send(stats);
                if matches!(action, Control::Stop) { return Ok(()); }
            },
            result = relays.next(), if !relays.is_empty() => {
                if let Some(Err(error)) = result {
                    // Peers may reset a connection when SQLx retires a pool.
                    // Unexpected connect/forwarding failures still fail cleanup.
                    if !matches!(error.kind(), std::io::ErrorKind::ConnectionReset
                        | std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::UnexpectedEof) {
                        return Err(error).context("database proxy forwarding failed");
                    }
                }
            },
            accepted = listener.accept() => {
                let (mut inbound, _) = accepted?;
                if !stats.open { stats.blocked += 1; continue; }
                ensure!(relays.len() < 32, "fixture database connection bound exceeded");
                stats.forwarded += 1;
                let upstream = upstream.clone();
                relays.push(Box::pin(async move {
                    let mut outbound = TcpStream::connect(upstream).await?;
                    tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await?;
                    Ok(())
                }));
            }
        }
    }
}

#[test]
fn proxy_rejects_decoded_routing_bypasses_without_changing_owned_database() -> Result<()> {
    ensure!(tcp_target("postgres://user:secret@localhost/owned?sslmode=disable").is_err());
    for query in [
        "h%6fst=other",
        "hostaddr=127.0.0.2",
        "port=99",
        "db%6eame=other",
        "service=other",
        "sslmode=verify-full",
        "ssl-mode=verify-full",
    ] {
        ensure!(
            tcp_target(&format!(
                "postgres://user:secret@localhost:5432/owned?{query}"
            ))
            .is_err()
        );
    }
    let (url, target) = tcp_target(
        "postgres://user:secret@localhost:5433/owned?sslmode=disable&application_name=fixture",
    )?;
    ensure!(target == ("localhost".into(), 5433));
    ensure!(url.path() == "/owned" && url.username() == "user" && url.password() == Some("secret"));
    ensure!(
        url.query_pairs()
            .any(|(key, value)| key == "application_name" && value == "fixture")
    );
    Ok(())
}
