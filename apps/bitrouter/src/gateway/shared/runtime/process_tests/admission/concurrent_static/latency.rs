//! An owned byte relay adds a small delay in both directions. It makes database
//! round trips observable without changing queries, deadlines or server state.

use super::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub(super) struct Relay {
    pub url: String,
    stop: CancellationToken,
    root: tokio::task::JoinHandle<Result<()>>,
    connections: Arc<AtomicUsize>,
}

impl Relay {
    pub async fn start(database: &str) -> Result<Self> {
        let mut url = url::Url::parse(database)?;
        let host = url
            .host_str()
            .context("explicit database host required")?
            .to_owned();
        let port = url.port().context("explicit database port required")?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        url.set_host(Some("127.0.0.1"))
            .map_err(|_| anyhow::anyhow!("invalid relay host"))?;
        url.set_port(Some(listener.local_addr()?.port()))
            .map_err(|_| anyhow::anyhow!("invalid relay port"))?;
        let stop = CancellationToken::new();
        let token = stop.clone();
        let connections = Arc::new(AtomicUsize::new(0));
        let count = connections.clone();
        let root = tokio::spawn(async move {
            let mut relays = JoinSet::new();
            let result = async {
                loop {
                    tokio::select! {
                        _ = token.cancelled() => break,
                        accepted = listener.accept() => {
                            let (incoming, _) = accepted?;
                            ensure!(relays.len() < 128, "fixture relay connection bound exceeded");
                            count.fetch_add(1, Ordering::SeqCst);
                            let host = host.clone();
                            let token = token.clone();
                            relays.spawn(async move {
                                tokio::select! {
                                    _ = token.cancelled() => Ok(()),
                                    result = relay(incoming, &host, port) => result,
                                }
                            });
                        }
                        joined = relays.join_next(), if !relays.is_empty() => { joined.context("missing relay")???; }
                    }
                }
                Ok(())
            }.await;
            token.cancel();
            let mut cleanup = Vec::new();
            while let Some(joined) = relays.join_next().await {
                cleanup.push(
                    joined
                        .map_err(anyhow::Error::from)
                        .and_then(|result| result),
                );
            }
            combine_results(result, cleanup)
        });
        Ok(Self {
            url: url.to_string(),
            stop,
            root,
            connections,
        })
    }

    pub async fn finish(mut self) -> Result<()> {
        self.stop.cancel();
        match tokio::time::timeout(Duration::from_secs(3), &mut self.root).await {
            Ok(joined) => joined??,
            Err(_) => {
                self.root.abort();
                let _ = self.root.await;
                anyhow::bail!("database relay shutdown timed out");
            }
        }
        ensure!(
            self.connections.load(Ordering::SeqCst) > 0,
            "database relay was unused"
        );
        Ok(())
    }
}

async fn relay(incoming: TcpStream, host: &str, port: u16) -> Result<()> {
    let outgoing = TcpStream::connect((host, port)).await?;
    incoming.set_nodelay(true)?;
    outgoing.set_nodelay(true)?;
    let (mut client_read, mut client_write) = incoming.into_split();
    let (mut server_read, mut server_write) = outgoing.into_split();
    let result = tokio::try_join!(
        forward(&mut client_read, &mut server_write),
        forward(&mut server_read, &mut client_write),
    );
    match result {
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

async fn forward(
    reader: &mut (impl AsyncRead + Unpin),
    writer: &mut (impl AsyncWrite + Unpin),
) -> std::io::Result<()> {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let bytes = reader.read(&mut buffer).await?;
        if bytes == 0 {
            return writer.shutdown().await;
        }
        // A per-chunk delay, not a claim of a calibrated network RTT. Tokio's
        // timer granularity and scheduler may add further latency.
        tokio::time::sleep(Duration::from_millis(1)).await;
        writer.write_all(&buffer[..bytes]).await?;
    }
}
