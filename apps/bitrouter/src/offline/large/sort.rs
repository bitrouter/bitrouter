//! Fixed-record external merge: bounded runs, two readers, duplicate preservation.
use super::{CapacityExceeded, Charge, Workspace, io};
use crate::offline::files;
use anyhow::{Context, Result, ensure};
use std::{
    fs::File,
    io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::Path,
};

const BUFFER: usize = 8192;

pub(crate) struct Records<'a, const WIDTH: usize> {
    workspace: &'a Workspace,
    spool: u64,
    runs: u64,
    len: u64,
    capacity: usize,
    values: Vec<[u8; WIDTH]>,
    charge: Charge,
}
impl<'a, const WIDTH: usize> Records<'a, WIDTH> {
    pub(crate) fn new(workspace: &'a Workspace, memory_bytes: usize) -> Result<Self> {
        ensure!(WIDTH > 0, "empty sort record");
        workspace.check()?;
        let overhead = WIDTH
            .checked_mul(6)
            .and_then(|bytes| bytes.checked_add(4 * BUFFER))
            .ok_or(CapacityExceeded)?;
        let capacity = memory_bytes.checked_sub(overhead).ok_or(CapacityExceeded)? / WIDTH;
        ensure!(capacity > 0, CapacityExceeded);
        let charge = workspace.reserve(memory_bytes)?;
        let mut values = Vec::new();
        values
            .try_reserve_exact(capacity)
            .map_err(|_| CapacityExceeded)?;
        ensure!(values.capacity() <= capacity, CapacityExceeded);
        Ok(Self {
            workspace,
            spool: workspace.next_spool()?,
            runs: 0,
            len: 0,
            capacity,
            values,
            charge,
        })
    }

    pub(crate) fn push(&mut self, record: [u8; WIDTH]) -> Result<()> {
        self.workspace.check()?;
        let next = self
            .len
            .checked_add(1)
            .context("sort record count overflow")?;
        if self.values.len() == self.capacity {
            self.flush()?;
        }
        self.values.push(record);
        self.len = next;
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        self.workspace.check()?;
        let next = self
            .runs
            .checked_add(1)
            .context("sort run count overflow")?;
        self.values.sort_unstable();
        let path = self.workspace.run_path(self.spool, 0, self.runs);
        let mut output = writer(&path, self.workspace)?;
        for record in &self.values {
            output.write_all(record)?;
        }
        complete(output)?;
        self.values.clear();
        self.runs = next;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<Sorted<'a, WIDTH>> {
        if !self.values.is_empty() || self.runs == 0 {
            self.flush()?;
        }
        // Run generation and merge buffers never overlap this resident chunk.
        drop(self.values);
        let mut pass = 0_u64;
        while self.runs > 1 {
            self.workspace.check()?;
            let next_pass = pass.checked_add(1).context("sort pass overflow")?;
            let mut input = 0_u64;
            let mut output = 0_u64;
            while input < self.runs {
                let left = self.workspace.run_path(self.spool, pass, input);
                let target = self.workspace.run_path(self.spool, next_pass, output);
                input = input.checked_add(1).context("sort run overflow")?;
                if input < self.runs {
                    let right = self.workspace.run_path(self.spool, pass, input);
                    merge::<WIDTH>(&left, &right, &target, self.workspace)?;
                    std::fs::remove_file(left)?;
                    std::fs::remove_file(right)?;
                    input = input.checked_add(1).context("sort run overflow")?;
                } else {
                    // Target is private to this spool/pass and has never existed.
                    std::fs::rename(left, target)?;
                }
                output = output.checked_add(1).context("sort run overflow")?;
            }
            self.runs = output;
            pass = next_pass;
        }
        let path = self.workspace.run_path(self.spool, pass, 0);
        let file = files::private_file(&path, u64::MAX)?;
        let expected = self
            .len
            .checked_mul(u64::try_from(WIDTH)?)
            .context("sort file size overflow")?;
        ensure!(
            file.metadata()?.len() == expected,
            "sort output size mismatch"
        );
        Ok(Sorted {
            workspace: self.workspace,
            input: BufReader::with_capacity(BUFFER, file),
            len: self.len,
            read: 0,
            _charge: self.charge,
        })
    }
}

pub(crate) struct Sorted<'a, const WIDTH: usize> {
    workspace: &'a Workspace,
    input: BufReader<File>,
    len: u64,
    read: u64,
    _charge: Charge,
}
impl<const WIDTH: usize> Sorted<'_, WIDTH> {
    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    /// Exact lookup for fixed-width spool binary searches; preserves next().
    pub(crate) fn read_at(&mut self, index: u64) -> Result<Option<[u8; WIDTH]>> {
        self.workspace.check()?;
        if index >= self.len {
            return Ok(None);
        }
        let width = u64::try_from(WIDTH)?;
        let offset = index.checked_mul(width).context("sort offset overflow")?;
        let current = self
            .read
            .checked_mul(width)
            .context("sort offset overflow")?;
        self.input.seek(SeekFrom::Start(offset))?;
        let value = record::<WIDTH>(&mut self.input)?.context("sort output truncated")?;
        self.input.seek(SeekFrom::Start(current))?;
        Ok(Some(value))
    }

    pub(crate) fn next(&mut self) -> Result<Option<[u8; WIDTH]>> {
        self.workspace.check()?;
        let record = record::<WIDTH>(&mut self.input)?;
        if record.is_some() {
            self.read = self
                .read
                .checked_add(1)
                .context("sort record count overflow")?;
            ensure!(self.read <= self.len, "sort output grew");
        } else {
            ensure!(self.read == self.len, "sort output truncated");
        }
        Ok(record)
    }
}

fn record<const WIDTH: usize>(input: &mut impl Read) -> Result<Option<[u8; WIDTH]>> {
    let mut record = [0; WIDTH];
    let count = loop {
        match input.read(&mut record[..1]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => break result?,
        }
    };
    if count == 0 {
        return Ok(None);
    }
    input.read_exact(&mut record[1..])?;
    Ok(Some(record))
}

struct Output<'a> {
    file: File,
    workspace: &'a Workspace,
}
impl Write for Output<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.workspace
            .probe(self.workspace.directory(), bytes.len() as u64)
            .map_err(std::io::Error::other)?;
        self.file.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
fn writer<'a>(path: &Path, workspace: &'a Workspace) -> Result<BufWriter<Output<'a>>> {
    workspace.probe(workspace.directory(), BUFFER as u64)?;
    Ok(BufWriter::with_capacity(
        BUFFER,
        Output {
            file: io::create_new(path)?,
            workspace,
        },
    ))
}
fn complete(mut output: BufWriter<Output<'_>>) -> Result<()> {
    output.flush()?;
    output.get_ref().file.sync_all()?;
    let workspace = output.get_ref().workspace;
    workspace.probe(workspace.directory(), 0)?;
    Ok(())
}

fn merge<const WIDTH: usize>(
    left: &Path,
    right: &Path,
    target: &Path,
    workspace: &Workspace,
) -> Result<()> {
    let mut left = BufReader::with_capacity(BUFFER, files::private_file(left, u64::MAX)?);
    let mut right = BufReader::with_capacity(BUFFER, files::private_file(right, u64::MAX)?);
    let mut output = writer(target, workspace)?;
    let mut a = record::<WIDTH>(&mut left)?;
    let mut b = record::<WIDTH>(&mut right)?;
    while a.is_some() || b.is_some() {
        workspace.check()?;
        let take_left = match (&a, &b) {
            (Some(a), Some(b)) => a <= b,
            (Some(_), None) => true,
            _ => false,
        };
        if take_left {
            output.write_all(a.as_ref().context("sort input missing")?)?;
            a = record::<WIDTH>(&mut left)?;
        } else {
            output.write_all(b.as_ref().context("sort input missing")?)?;
            b = record::<WIDTH>(&mut right)?;
        }
    }
    complete(output)
}
