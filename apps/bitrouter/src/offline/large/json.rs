//! Count one concrete artifact's serialization before allocating its output.
use super::{CapacityExceeded, Charge, Workspace};
use anyhow::{Result, ensure};
use std::io::Write;

struct Count {
    bytes: usize,
    limit: usize,
    exhausted: bool,
}
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let Some(next) = self
            .bytes
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
        else {
            self.exhausted = true;
            return Err(std::io::Error::other(CapacityExceeded));
        };
        self.bytes = next;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn encode(
    value: &impl serde::Serialize,
    workspace: &Workspace,
) -> Result<(Vec<u8>, Charge)> {
    workspace.check()?;
    let mut count = Count {
        bytes: 0,
        limit: workspace.available() / 2,
        exhausted: false,
    };
    let counted = serde_json::to_writer_pretty(&mut count, value);
    ensure!(!count.exhausted, CapacityExceeded);
    counted?;
    let capacity = count.bytes.checked_mul(2).ok_or(CapacityExceeded)?;
    let charge = workspace.reserve(capacity)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(count.bytes)
        .map_err(|_| CapacityExceeded)?;
    serde_json::to_writer_pretty(&mut bytes, value)?;
    ensure!(
        bytes.len() == count.bytes && bytes.capacity() <= capacity,
        "artifact serialization changed"
    );
    workspace.check()?;
    Ok((bytes, charge))
}
