//! Bounded local comparisons of separately built gateway processes.

#[cfg(unix)]
mod host;
#[cfg(unix)]
mod metrics;
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
    use std::path::PathBuf;

    diagnostic_phase(format_args!("run start"));
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
        diagnostic_phase(format_args!("fingerprint slot={label} start"));
        let path = std::fs::canonicalize(path)?;
        let path = path.to_str().context("artifact path is not UTF-8")?;
        let (bytes, sha256) = fingerprint(std::path::Path::new(path))?;
        diagnostic_phase(format_args!("fingerprint slot={label} end bytes={bytes}"));
        artifacts.push(serde_json::json!({
            "label":label,"path":path,"bytes":bytes,
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
            diagnostic_phase(format_args!("verify round={round} slot={label} start"));
            verify_artifact(&path, artifact)?;
            diagnostic_phase(format_args!("verify round={round} slot={label} end"));
            diagnostic_phase(format_args!("provider_setup start"));
            let sample = workload::measure(&path, label, round).await?;
            verify_artifact(&path, artifact)?;
            eprintln!("{}", serde_json::to_string(&sample)?);
            samples.push(sample);
        }
    }
    let report = serde_json::json!({
        "schema":"bitrouter.gateway-local-measurements.v2",
        "platform":std::env::consts::OS,"architecture":std::env::consts::ARCH,
        "workload":{"warmup_requests":workload::WARMUP,"requests_per_mode":workload::REQUESTS,"concurrency":workload::CONCURRENCY,"rss_unit":"KiB","provider":"local wiremock; fixed small response", "observation_interval_ms":workload::OBSERVATION_INTERVAL.as_millis(), "failure_policy":"any refused or failed request invalidates the run; no completed report", "ttft_definition":"request start to first complete SSE frame with nonempty content delta; immediate fixed response, not provider generation speed"},
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

#[cfg(unix)]
fn diagnostic_phase(phase: std::fmt::Arguments<'_>) {
    use std::io::Write;
    use std::sync::OnceLock;
    use std::time::Instant;

    // Fixed phase boundaries only; diagnostic output never changes the result.
    static STARTED: OnceLock<Instant> = OnceLock::new();
    let elapsed = STARTED.get_or_init(Instant::now).elapsed();
    let _ = writeln!(
        std::io::stderr().lock(),
        "measurement_phase {phase} elapsed_ms={:.3}",
        elapsed.as_secs_f64() * 1000.0
    );
}

#[cfg(unix)]
fn fingerprint(path: &std::path::Path) -> anyhow::Result<(u64, String)> {
    use anyhow::Context;
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut bytes = 0_u64;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(count as u64)
            .context("artifact size overflow")?;
        digest.update(&buffer[..count]);
    }
    let mut sha256 = String::new();
    for byte in digest.finalize() {
        write!(sha256, "{byte:02x}")?;
    }
    Ok((bytes, sha256))
}

#[cfg(unix)]
fn verify_artifact(path: &std::path::Path, evidence: &serde_json::Value) -> anyhow::Result<()> {
    let (bytes, sha256) = fingerprint(path)?;
    anyhow::ensure!(
        evidence["bytes"] == bytes && evidence["sha256"] == sha256,
        "artifact changed during qualification: {}",
        path.display()
    );
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn changed_artifact_invalidates_evidence() -> anyhow::Result<()> {
        let file = tempfile::NamedTempFile::new()?;
        std::fs::write(file.path(), b"abc")?;
        let (bytes, sha256) = super::fingerprint(file.path())?;
        assert_eq!(
            sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let evidence = serde_json::json!({"bytes":bytes,"sha256":sha256});
        super::verify_artifact(file.path(), &evidence)?;
        std::fs::write(file.path(), b"abd")?;
        assert!(super::verify_artifact(file.path(), &evidence).is_err());
        Ok(())
    }
}

#[cfg(not(unix))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("this local RSS measurement fixture requires Unix")
}
