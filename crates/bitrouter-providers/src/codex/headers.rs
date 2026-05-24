//! Header constants for the OpenAI Codex (ChatGPT subscription) provider.
//!
//! Codex's backend at `chatgpt.com/backend-api/codex` rejects requests that
//! don't carry the integration headers an official CLI client would send.
//! We mirror what OpenAI's Codex CLI ships with — `chatgpt-account-id`
//! sourced from the OAuth access-token's JWT claims, `OpenAI-Beta` to opt
//! into the streamed-Responses path, and an `originator` tag identifying
//! the client.
//!
//! Header values cribbed from the OpenCode reference at
//! `packages/opencode/src/plugin/codex.ts` (line 108: `originator: "opencode"`)
//! and the OpenClaw codex device-code module
//! (`extensions/openai/openai-codex-device-code.ts`, line 15:
//! `originator: "openclaw"`). We send `bitrouter` for our own
//! attribution.

/// `originator` value. Codex's backend tags telemetry by originator; using
/// `bitrouter` lets the OpenAI side distinguish bitrouter traffic from
/// other Codex clients.
pub const ORIGINATOR: &str = "bitrouter";

/// `OpenAI-Beta` value to opt into the experimental Responses API surface
/// that Codex's backend speaks.
pub const OPENAI_BETA: &str = "responses=experimental";
