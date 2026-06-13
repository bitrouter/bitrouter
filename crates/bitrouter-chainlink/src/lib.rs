//! # bitrouter-chainlink
//!
//! Outbound [`Executor`](bitrouter_sdk::language_model::Executor) for the
//! Chainlink Confidential Inference API — a TEE-backed (AWS Nitro Enclave),
//! asynchronous submit-then-poll inference service.
//!
//! This crate bridges that async, non-OpenAI API onto BitRouter's canonical
//! pipeline, so any of the four inbound protocols (chat-completions, responses,
//! Anthropic messages, Gemini generateContent) can drive it. It is a
//! hackathon-demo provider, wired into `apps/bitrouter` behind an off-by-default
//! `chainlink-demo` feature and kept out of `provider-registry` / cloud.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod wire;

/// The custom protocol id used in config (`api_protocol: chainlink_confidential`)
/// and on the DispatchExecutor key.
pub const PROTOCOL: &str = "chainlink_confidential";
