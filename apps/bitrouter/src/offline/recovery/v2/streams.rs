//! Single-record parsing with explicit overlap allowance and complete EOF checks.

use super::format::{
    self, ProvisioningHeader, ProvisioningRecord, RetirementHeader, RetirementRecord,
};
use crate::offline::large::{Charge, Workspace};
use crate::tenancy::resources::recovery::v2::ApprovedResource;
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    identity::InstallationId, operation::OperationStatus, validation::Validate,
};
use serde::de::DeserializeOwned;
use std::{
    fs::File,
    io::{BufRead, BufReader},
};

#[derive(Debug, thiserror::Error)]
#[error("offline record working capacity exceeded")]
pub(crate) struct WorkingCapacity;

/// One admitted parser phase. The caller reserves `bytes` in the command's
/// aggregate working allowance. Buffer and decoded-record reservations have
/// independent lifetimes, so retained records cannot outlive their charge.
pub(crate) struct JsonLines<'a> {
    input: BufReader<File>,
    bytes: usize,
    line: Vec<u8>,
    records: u64,
    workspace: &'a Workspace,
    buffer_charge: Charge,
}

/// The decode reservation follows the record, including when a caller retains
/// it while advancing a different stream. It is never a lifetime record quota.
pub(crate) struct Record<T> {
    pub value: T,
    _charge: Charge,
}

impl<'a> JsonLines<'a> {
    /// `file` must already be private, anchored and checked against SQL aliases.
    pub(crate) fn new(file: File, workspace: &'a Workspace) -> Result<Self> {
        let bytes = workspace.available();
        ensure!(
            bytes > 0 && bytes <= isize::MAX as usize,
            "invalid record allowance"
        );
        let buffer = (bytes / 16).clamp(1, 4096);
        let buffer_charge = workspace.reserve(buffer.checked_add(8192).ok_or(WorkingCapacity)?)?;
        Ok(Self {
            input: BufReader::with_capacity(buffer, file),
            bytes,
            line: Vec::new(),
            records: 0,
            workspace,
            buffer_charge,
        })
    }

    pub(crate) fn next<T: DeserializeOwned>(
        &mut self,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<Option<Record<T>>> {
        check()?;
        self.line.clear();
        loop {
            check()?;
            let source = self.input.fill_buf()?;
            if source.is_empty() {
                ensure!(self.line.is_empty(), "JSON-lines record lacks final LF");
                return Ok(None);
            }
            let end = source.iter().position(|b| *b == b'\n');
            let count = end.map_or(source.len(), |n| n + 1);
            let needed = self.line.len().checked_add(count).ok_or(WorkingCapacity)?;
            if needed > self.bytes / 8 {
                return Err(WorkingCapacity.into());
            }
            if needed > self.line.capacity() {
                self.buffer_charge.grow(needed - self.line.capacity())?;
                self.line
                    .try_reserve_exact(needed - self.line.len())
                    .map_err(|_| WorkingCapacity)?;
                if self.line.capacity() > self.bytes / 8 {
                    return Err(WorkingCapacity.into());
                }
            }
            self.line.extend_from_slice(&source[..count]);
            self.input.consume(count);
            if end.is_some() {
                break;
            }
        }
        ensure!(self.line.len() > 1, "empty JSON-lines record");
        // Count possible decoded nodes before serde allocates objects/arrays.
        // JSON syntax and exact DTO fields are checked by serde below. String
        // contents cannot manufacture nodes; escapes stay inside the string.
        let mut quoted = false;
        let mut escaped = false;
        let mut nodes = 0usize;
        let mut depth = 0usize;
        for byte in &self.line {
            if quoted {
                if escaped {
                    escaped = false;
                } else if *byte == b'\\' {
                    escaped = true;
                } else if *byte == b'"' {
                    quoted = false;
                }
            } else {
                match byte {
                    b'"' => {
                        quoted = true;
                        nodes = nodes.checked_add(1).ok_or(WorkingCapacity)?;
                    }
                    b'{' | b'[' => {
                        depth = depth.checked_add(1).ok_or(WorkingCapacity)?;
                        ensure!(depth <= 64, "JSON nesting exceeds object profile");
                        nodes = nodes.checked_add(1).ok_or(WorkingCapacity)?;
                    }
                    b'}' | b']' => {
                        depth = depth.saturating_sub(1);
                    }
                    b',' | b':' => {
                        nodes = nodes.checked_add(1).ok_or(WorkingCapacity)?;
                    }
                    _ => {}
                }
            }
        }
        let copies = self
            .line
            .len()
            .checked_mul(4)
            .and_then(|n| n.checked_add(nodes.checked_mul(256)?))
            .and_then(|n| n.checked_add(std::mem::size_of::<T>().checked_mul(2)?))
            .and_then(|n| n.checked_add(self.line.capacity()))
            .and_then(|n| n.checked_add(self.input.capacity()))
            .ok_or(WorkingCapacity)?;
        if copies > self.bytes {
            return Err(WorkingCapacity.into());
        }
        let charge = self.workspace.reserve(copies)?;
        let value =
            serde_json::from_slice(&self.line).context("invalid offline JSON-lines record")?;
        self.records = self
            .records
            .checked_add(1)
            .context("record count overflow")?;
        Ok(Some(Record {
            value,
            _charge: charge,
        }))
    }

    pub(crate) fn finish(&mut self, check: &(dyn Fn() -> Result<()> + Sync)) -> Result<()> {
        check()?;
        ensure!(
            self.input.fill_buf()?.is_empty(),
            "trailing JSON-lines record or bytes"
        );
        Ok(())
    }
}

pub(crate) struct Provisioning<'a> {
    lines: JsonLines<'a>,
    pub header: ProvisioningHeader,
    counts: [u64; 4],
    previous_configuration: Option<(String, String)>,
    previous_active: Option<String>,
    previous_resource: Option<[u8; 416]>,
}

impl<'a> Provisioning<'a> {
    pub(crate) fn new(
        mut lines: JsonLines<'a>,
        installation: &InstallationId,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<Self> {
        let header = lines
            .next::<ProvisioningHeader>(check)?
            .context("provisioning header missing")?
            .value;
        header.validate(installation)?;
        Ok(Self {
            lines,
            header,
            counts: [0; 4],
            previous_configuration: None,
            previous_active: None,
            previous_resource: None,
        })
    }

    pub(crate) fn next(
        &mut self,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<Option<Record<ProvisioningRecord>>> {
        let limits = [
            self.header.operation_count,
            self.header.configuration_count,
            self.header.active_count,
            self.header.resource_count,
        ];
        let section = self
            .counts
            .iter()
            .zip(limits)
            .position(|(seen, limit)| *seen < limit);
        let Some(section) = section else {
            self.lines.finish(check)?;
            return Ok(None);
        };
        let record = self
            .lines
            .next::<ProvisioningRecord>(check)?
            .context("incomplete provisioning stream")?;
        match &record.value {
            ProvisioningRecord::Operation { result, .. } => {
                ensure!(section == 0, "wrong provisioning section");
                result.validate()?;
                ensure!(
                    result.status == OperationStatus::Applied
                        && result
                            .authority_revision
                            .is_some_and(|r| r.get() == self.counts[0] + 1),
                    "noncontiguous provisioning operation revision"
                );
            }
            ProvisioningRecord::Configuration { revision } => {
                ensure!(section == 1, "wrong provisioning section");
                revision.validate()?;
                let key = (
                    revision.tenant_id.to_string(),
                    revision.revision_id.to_string(),
                );
                ensure!(
                    self.previous_configuration
                        .as_ref()
                        .is_none_or(|old| old < &key),
                    "configuration order or duplicate"
                );
                self.previous_configuration = Some(key);
            }
            ProvisioningRecord::Active {
                tenant_id,
                authority_revision,
                ..
            } => {
                ensure!(
                    section == 2 && authority_revision.get() <= self.header.operation_count,
                    "active section or revision"
                );
                ensure!(
                    self.previous_active
                        .as_ref()
                        .is_none_or(|old| old.as_str() < tenant_id.as_str()),
                    "active order or duplicate"
                );
                self.previous_active = Some(tenant_id.to_string());
            }
            ProvisioningRecord::Resource { reference } => {
                ensure!(section == 3, "wrong provisioning section");
                let key = format::resource_key(reference)?;
                ensure!(
                    self.previous_resource.is_none_or(|old| old < key),
                    "resource order or duplicate"
                );
                self.previous_resource = Some(key);
            }
        }
        self.counts[section] = self.counts[section]
            .checked_add(1)
            .context("provisioning count overflow")?;
        Ok(Some(record))
    }
}

pub(crate) struct Retirement<'a> {
    lines: JsonLines<'a>,
    pub header: RetirementHeader,
    writers: u64,
    files: u64,
    previous_writer: Option<String>,
    previous_file: Option<std::path::PathBuf>,
}

pub(crate) struct Approval<'a> {
    lines: JsonLines<'a>,
    pub header: format::ApprovalHeader,
    seen: u64,
    previous: Option<[u8; 416]>,
}

impl<'a> Approval<'a> {
    pub(crate) fn new(
        mut lines: JsonLines<'a>,
        installation: &InstallationId,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<Self> {
        let header = lines
            .next::<format::ApprovalHeader>(check)?
            .context("approval header missing")?
            .value;
        ensure!(
            header.format == format::APPROVAL && &header.installation_id == installation,
            "approval identity mismatch"
        );
        i64::try_from(header.resource_count)?;
        Ok(Self {
            lines,
            header,
            seen: 0,
            previous: None,
        })
    }

    pub(crate) fn next(
        &mut self,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<Option<Record<ApprovedResource>>> {
        if self.seen == self.header.resource_count {
            self.lines.finish(check)?;
            return Ok(None);
        }
        let record = self
            .lines
            .next::<ApprovedResource>(check)?
            .context("incomplete approval stream")?;
        let key = format::resource_key(&record.value.reference)?;
        ensure!(
            self.previous.is_none_or(|old| old < key),
            "approval order or duplicate"
        );
        self.previous = Some(key);
        self.seen = self
            .seen
            .checked_add(1)
            .context("approval count overflow")?;
        Ok(Some(record))
    }
}

impl<'a> Retirement<'a> {
    pub(crate) fn new(
        mut lines: JsonLines<'a>,
        source: &InstallationId,
        evidence: &str,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<Self> {
        let header = lines
            .next::<RetirementHeader>(check)?
            .context("retirement header missing")?
            .value;
        header.validate(source, evidence)?;
        Ok(Self {
            lines,
            header,
            writers: 0,
            files: 0,
            previous_writer: None,
            previous_file: None,
        })
    }

    pub(crate) fn next(
        &mut self,
        check: &(dyn Fn() -> Result<()> + Sync),
    ) -> Result<Option<Record<RetirementRecord>>> {
        if self.writers == self.header.writer_count && self.files == self.header.selected_file_count
        {
            self.lines.finish(check)?;
            return Ok(None);
        }
        let record = self
            .lines
            .next::<RetirementRecord>(check)?
            .context("incomplete retirement stream")?;
        match &record.value {
            RetirementRecord::Writer {
                id,
                process_identity,
                restart_source,
                retired,
            } => {
                ensure!(
                    self.writers < self.header.writer_count && *retired,
                    "writer section or retirement assertion"
                );
                for label in [id, process_identity, restart_source] {
                    format::label(label)?;
                }
                ensure!(
                    self.previous_writer.as_ref().is_none_or(|old| old < id),
                    "writer order or duplicate"
                );
                self.previous_writer = Some(id.clone());
                self.writers = self
                    .writers
                    .checked_add(1)
                    .context("writer count overflow")?;
            }
            RetirementRecord::File {
                path,
                writer,
                keyed_digest,
                ..
            } => {
                ensure!(
                    self.writers == self.header.writer_count
                        && self.files < self.header.selected_file_count,
                    "selected file section"
                );
                super::super::plan::absolute(path)?;
                ensure!(
                    path.to_str().is_some() && path.canonicalize()? == *path,
                    "selected file path must be canonical UTF-8"
                );
                format::label(writer)?;
                format::digest(keyed_digest)?;
                ensure!(
                    self.previous_file.as_ref().is_none_or(|old| old < path),
                    "selected file order or duplicate"
                );
                self.previous_file = Some(path.clone());
                self.files = self
                    .files
                    .checked_add(1)
                    .context("selected file count overflow")?;
            }
        }
        Ok(Some(record))
    }
}

#[cfg(all(test, unix))]
mod tests;
