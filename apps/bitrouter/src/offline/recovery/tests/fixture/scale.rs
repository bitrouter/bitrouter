//! Fixed-buffer producers shared by the explicit offline scale qualifications.

use crate::offline::recovery::{plan::Anchor, v2::format::ProvisioningRecord};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::PrincipalPut,
    identity::{AuthorityRevision, InstallationId, PrincipalId},
    operation::OperationStatus,
};
use bitrouter_sdk::tenancy::TenancyClient;
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::{BufWriter, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

const BUFFER: usize = 64 * 1024;

pub(crate) mod observation;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct FileWitness {
    pub bytes: u64,
    pub sha256: String,
}

/// Hash bytes without retaining the file. The caller owns SQLite/file custody.
pub(crate) fn witness(path: &Path) -> Result<FileWitness> {
    let mut file = File::open(path)?;
    let expected = file.metadata()?.len();
    let mut buffer = [0_u8; BUFFER];
    let mut hash = Sha256::new();
    let mut bytes = 0_u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(count)?)
            .context("fixture file length overflow")?;
        ensure!(bytes <= expected, "fixture file grew while hashing");
        hash.update(&buffer[..count]);
    }
    ensure!(
        bytes == expected && file.metadata()?.len() == expected,
        "fixture file changed while hashing"
    );
    Ok(FileWitness {
        bytes,
        sha256: hex::encode(hash.finalize()),
    })
}

/// The caller writes one bounded DTO at a time and owns section ordering.
pub(crate) struct JsonLines {
    path: PathBuf,
    file: BufWriter<File>,
}

impl JsonLines {
    pub(crate) fn create(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "absolute fixture stream path required");
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            path: path.into(),
            file: BufWriter::with_capacity(BUFFER, file),
        })
    }

    pub(crate) fn record(&mut self, value: &impl serde::Serialize) -> Result<()> {
        serde_json::to_writer(&mut self.file, value)?;
        self.file.write_all(b"\n")?;
        Ok(())
    }

    /// Append an already closed section, never accumulating a complete stream.
    pub(crate) fn append_section(&mut self, path: &Path) -> Result<()> {
        ensure!(path != self.path, "fixture stream cannot append itself");
        let mut input = File::open(path)?;
        let expected = input.metadata()?.len();
        let mut total = 0_u64;
        let mut buffer = [0_u8; BUFFER];
        let mut last = None;
        loop {
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(u64::try_from(count)?)
                .context("fixture section length overflow")?;
            ensure!(total <= expected, "fixture section grew");
            self.file.write_all(&buffer[..count])?;
            last = Some(buffer[count - 1]);
        }
        ensure!(
            total == expected && input.metadata()?.len() == expected,
            "fixture section changed"
        );
        ensure!(
            last.is_none_or(|byte| byte == b'\n'),
            "fixture section lacks final newline"
        );
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<(Anchor, FileWitness)> {
        self.file.flush()?;
        self.file.get_ref().sync_all()?;
        drop(self.file);
        let observed = witness(&self.path)?;
        Ok((
            Anchor {
                path: self.path,
                sha256: observed.sha256.clone(),
            },
            observed,
        ))
    }
}

/// Produce genuine retained operations; the count changes loop work, not memory.
pub(crate) async fn append_principals(
    client: &TenancyClient,
    installation: &InstallationId,
    caller: &PrincipalId,
    first: u64,
    count: u64,
    operations: &mut JsonLines,
) -> Result<AuthorityRevision> {
    ensure!(count > 0, "principal tail must be nonempty");
    let end = first
        .checked_add(count)
        .context("principal ordinal overflow")?;
    let mut previous = None::<AuthorityRevision>;
    for ordinal in first..end {
        let principal_text = format!("{:0>128}", format!("scale-principal-{ordinal:020}"));
        let operation_text = format!("{:0>128}", format!("scale-operation-{ordinal:020}"));
        ensure!(principal_text.len() == 128 && operation_text.len() == 128);
        let principal: PrincipalId = principal_text.as_str().try_into()?;
        let operation_id = operation_text.as_str().try_into()?;
        let applied = client
            .put_principal(
                &principal,
                &PrincipalPut {
                    operation_id,
                    expected_revision: None,
                    issuer: "offline-scale-fixture".into(),
                    subject: principal_text,
                    installation_id: installation.clone(),
                },
            )
            .await?;
        let retained = client
            .operation(installation, &applied.operation_id)
            .await?;
        ensure!(
            applied.status == OperationStatus::Applied
                && retained.status == OperationStatus::Applied
                && retained.resource_id.as_ref().map(|id| id.as_str()) == Some(principal.as_str())
                && serde_json::to_value(&retained)? == serde_json::to_value(&applied)?,
            "principal tail did not retain the actual successful result"
        );
        let revision = retained
            .authority_revision
            .context("applied revision missing")?;
        if let Some(previous) = previous {
            ensure!(
                previous.get().checked_add(1) == Some(revision.get()),
                "principal tail authority revisions are not consecutive"
            );
        }
        operations.record(&ProvisioningRecord::Operation {
            principal_id: caller.clone(),
            result: retained,
        })?;
        previous = Some(revision);
    }
    previous.context("principal tail result missing")
}
