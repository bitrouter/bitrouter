use super::{RegistrationInput, queue};
use crate::tenancy::Service;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::path::Path;
use tokio::io::AsyncReadExt;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: u32,
    registrations: Vec<Entry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    credential_env: String,
    registration: RegistrationInput,
}

pub(in crate::tenancy) async fn initialize(service: &Service, path: Option<&Path>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let bytes = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let file = tokio::fs::File::open(path).await?;
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() <= 1024 * 1024,
            "learning registration manifest capacity"
        );
        Ok::<_, anyhow::Error>(bytes)
    })
    .await
    .context("learning registration manifest deadline")??;
    let manifest: Manifest = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid learning registration manifest"))?;
    ensure!(
        manifest.format == 1 && manifest.registrations.len() <= super::MAX_DUE,
        "learning registration manifest format or capacity"
    );
    for entry in manifest.registrations {
        ensure!(
            !entry.credential_env.is_empty()
                && entry.credential_env.len() <= 128
                && entry
                    .credential_env
                    .bytes()
                    .enumerate()
                    .all(|(index, byte)| byte == b'_'
                        || byte.is_ascii_alphabetic()
                        || (index > 0 && byte.is_ascii_digit())),
            "invalid learning credential environment name"
        );
        let token = std::env::var(&entry.credential_env)
            .map_err(|_| anyhow::anyhow!("learning registration credential unavailable"))?;
        // Only a real tenant API credential is accepted; operator possession of
        // this file is not a tenant grant, and the secret is not queued/stored.
        let caller = service
            .authenticate(&token)
            .map_err(|_| anyhow::anyhow!("learning registration authentication failed"))?;
        drop(token);
        let result = queue::register(service, caller, entry.registration)
            .await
            .map_err(|error| {
                anyhow::anyhow!("learning registration rejected: {:?}", error.0.code)
            })?;
        tracing::info!(operation = %result.operation_id, "owned learning registration retained");
    }
    Ok(())
}
