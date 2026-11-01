//! One bounded payload: exact receipt bytes followed by optional exact usage bytes.
use anyhow::{Context, Result, ensure};

use super::Charge;

pub(super) const MAX_PAYLOAD: usize =
    super::super::MAX_RECEIPT + bitrouter_ai::tenancy::usage_evidence::MAX_BYTES;
// These are codec workspace allowances, not history or directory limits.
const ENCODE_WORKSPACE: usize = 8 * 1024 * 1024;
const DECODE_WORKSPACE: usize = 256 * 1024;

pub(super) struct Encoded {
    pub version: i64,
    pub codec: i64,
    pub receipt_bytes: usize,
    pub usage_bytes: Option<usize>,
    pub payload: Vec<u8>,
}

pub(super) fn plaintext_len(receipt: usize, usage: Option<usize>) -> Result<usize> {
    ensure!(
        (1..=super::super::MAX_RECEIPT).contains(&receipt)
            && usage.is_none_or(|n| (1..=bitrouter_ai::tenancy::usage_evidence::MAX_BYTES).contains(&n)),
        "receipt archive component bounds"
    );
    receipt
        .checked_add(usage.unwrap_or(0))
        .context("receipt archive length overflow")
}

impl Encoded {
    pub fn encode(receipt: &str, usage: Option<&str>, charge: &Charge<'_>) -> Result<Self> {
        let receipt_bytes = receipt.len();
        let usage_bytes = usage.map(str::len);
        let size = plaintext_len(receipt_bytes, usage_bytes)?;
        let capacity = zstd::zstd_safe::compress_bound(size);
        charge(
            0,
            size.checked_add(capacity)
                .and_then(|n| n.checked_add(ENCODE_WORKSPACE))
                .context("receipt archive compression allocation")?,
        )?;
        let mut plain = Vec::with_capacity(size);
        plain.extend_from_slice(receipt.as_bytes());
        if let Some(usage) = usage {
            plain.extend_from_slice(usage.as_bytes());
        }
        // Level 1, bounded input, no dictionary or parallel workers. Explicitly
        // bound the context's tables/window as well as the output allocation.
        let mut compressor = zstd::bulk::Compressor::new(1)?;
        for parameter in [
            zstd::zstd_safe::CParameter::WindowLog(20),
            zstd::zstd_safe::CParameter::HashLog(17),
            zstd::zstd_safe::CParameter::ChainLog(16),
        ] {
            compressor.set_parameter(parameter)?;
        }
        let mut compressed = Vec::with_capacity(capacity);
        compressor.compress_to_buffer(&plain, &mut compressed)?;
        let (codec, payload) = if compressed.len() < plain.len() {
            (1, compressed)
        } else {
            (0, plain)
        };
        charge(0, 0)?;
        Ok(Self {
            version: 1,
            codec,
            receipt_bytes,
            usage_bytes,
            payload,
        })
    }

    pub fn decode(self, charge: &Charge<'_>) -> Result<(String, Option<String>)> {
        ensure!(self.version == 1, "receipt archive version");
        let size = plaintext_len(self.receipt_bytes, self.usage_bytes)?;
        ensure!(
            !self.payload.is_empty() && self.payload.len() <= MAX_PAYLOAD,
            "receipt archive stored length"
        );
        // Output, split strings, parsed document and canonical encoding coexist.
        charge(
            0,
            size.checked_mul(4)
                .and_then(|n| n.checked_add(DECODE_WORKSPACE))
                .context("receipt archive decode allocation")?,
        )?;
        let plain = match self.codec {
            0 => {
                ensure!(self.payload.len() == size, "receipt archive raw length");
                self.payload
            }
            1 => {
                // Ordinary magic excludes skippable frames. Exact frame extent
                // excludes concatenation and trailing bytes even if decoding
                // the first frame would reproduce the expected digest.
                // https://github.com/facebook/zstd/blob/dev/doc/zstd_compression_format.md
                ensure!(
                    self.payload.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]),
                    "receipt archive frame magic"
                );
                let frame_size = zstd::zstd_safe::find_frame_compressed_size(&self.payload)
                    .map_err(|_| anyhow::anyhow!("receipt archive frame extent"))?;
                ensure!(
                    frame_size == self.payload.len()
                        && zstd::zstd_safe::get_dict_id_from_frame(&self.payload).is_none(),
                    "receipt archive frame tail or dictionary"
                );
                let declared = zstd::zstd_safe::get_frame_content_size(&self.payload)
                    .map_err(|_| anyhow::anyhow!("receipt archive frame size"))?;
                ensure!(
                    declared.is_none_or(|n| n == size as u64),
                    "receipt archive frame length"
                );
                // One-shot decompression uses caller-bounded output; it does not
                // allocate a streaming window from an untrusted frame header.
                zstd::bulk::decompress(&self.payload, size)?
            }
            _ => anyhow::bail!("receipt archive codec"),
        };
        ensure!(plain.len() == size, "receipt archive plaintext length");
        let receipt = String::from_utf8(plain[..self.receipt_bytes].to_vec())?;
        let usage = self
            .usage_bytes
            .map(|_| String::from_utf8(plain[self.receipt_bytes..].to_vec()))
            .transpose()?;
        charge(0, 0)?;
        Ok((receipt, usage))
    }
}
