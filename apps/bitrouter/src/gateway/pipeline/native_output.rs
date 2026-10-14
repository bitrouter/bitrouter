//! Canonical output admission before durable-report copies. Executor-owned
//! allocations and the original settlement evidence retain their own lifetimes.

use super::native::{NativeOutputRejection, NativeOutputUsage};
use bitrouter_ai::types::GenerateResult;

pub(super) fn fits(result: &impl serde::Serialize, limit: u64) -> bool {
    serde_json::to_writer(BoundedCounter(limit), result).is_ok()
}

struct BoundedCounter(u64);

impl std::io::Write for BoundedCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.checked_sub(bytes.len() as u64).ok_or_else(|| {
            std::io::Error::other("canonical model output exceeds its admitted byte limit")
        })?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn rejection(result: &GenerateResult, byte_limit: u64) -> NativeOutputRejection {
    NativeOutputRejection {
        byte_limit,
        usage: result.usage.as_ref().map(|usage| NativeOutputUsage {
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            reasoning_tokens: usage.reasoning_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            web_search_count: usage.web_search_count,
            origin: usage.origin,
        }),
    }
}

pub(super) fn empty_result() -> GenerateResult {
    GenerateResult {
        content: Vec::new(),
        usage: None,
        finish_reason: None,
        response_id: None,
        stop_details: None,
        provider_metadata: Default::default(),
    }
}
