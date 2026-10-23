//! Bounded local comparisons of separately built gateway processes.

#[cfg(unix)]
mod host;
#[cfg(unix)]
mod workload;

#[cfg(unix)]
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> anyhow::Result<()> {
    use tokio::signal::unix::{SignalKind, signal};

    // Register before spawning any gateway. Cancelling run drops its Host guard,
    // which kills/reaps only the owned child before removing its private files.
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    tokio::select! {
        result = run() => result,
        _ = interrupt.recv() => anyhow::bail!("measurement interrupted"),
        _ = terminate.recv() => anyhow::bail!("measurement terminated"),
    }
}

#[cfg(unix)]
async fn run() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    use std::path::PathBuf;

    let arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    ensure!(
        arguments.len() == 4,
        "usage: gateway-measurements BASELINE FULL GATEWAY OUTPUT_JSON"
    );
    let mut artifacts = Vec::new();
    for (label, path) in ["baseline", "full", "gateway"]
        .into_iter()
        .zip(&arguments[..3])
    {
        let path = std::fs::canonicalize(path)?;
        let path = path.to_str().context("artifact path is not UTF-8")?;
        let bytes = std::fs::read(path)?;
        let mut sha256 = String::new();
        for byte in Sha256::digest(&bytes) {
            write!(sha256, "{byte:02x}")?;
        }
        artifacts.push(serde_json::json!({
            "label":label,"path":path,"bytes":bytes.len(),
            "sha256":sha256
        }));
    }
    let mut samples = Vec::new();
    // Alternate ordering to expose warm-cache/order effects. Every sample uses
    // a new private database and only one gateway runs at a time.
    for (round, order) in [[0, 1, 2], [2, 1, 0], [1, 0, 2]].into_iter().enumerate() {
        for index in order {
            let artifact = &artifacts[index];
            let label = artifact["label"].as_str().context("label missing")?;
            let path = PathBuf::from(artifact["path"].as_str().context("path missing")?);
            let sample = workload::measure(&path, label, round).await?;
            eprintln!("{}", serde_json::to_string(&sample)?);
            samples.push(sample);
        }
    }
    let report = serde_json::json!({
        "schema":"bitrouter.gateway-local-measurements.v1",
        "platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,
        "workload":{"warmup_requests":workload::WARMUP,"requests_per_mode":workload::REQUESTS,"concurrency":workload::CONCURRENCY,"rss_unit":"KiB","provider":"local wiremock; fixed small response"},
        "artifacts":artifacts,"samples":samples
    });
    let output = PathBuf::from(&arguments[3]);
    ensure!(output.is_absolute(), "OUTPUT_JSON must be absolute");
    // Do not replace evidence from an earlier measurement run.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    serde_json::to_writer_pretty(file, &report)?;
    Ok(())
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("this local RSS measurement fixture requires Unix")
}
