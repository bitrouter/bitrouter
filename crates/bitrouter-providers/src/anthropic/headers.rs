//! Header constants for the Anthropic provider's request shaping.
//!
//! These mirror the values [`MessagesTransport::authorise`] would have set
//! by default — pulled out here so the OAuth-aware `AuthApplier` can apply
//! the same constants when no OAuth credential is in play.
//!
//! [`MessagesTransport::authorise`]: bitrouter_sdk::language_model::protocol::messages

/// The pinned `anthropic-version` value. Anthropic's API requires every
/// request to declare a version (`https://docs.anthropic.com/en/api/versioning`);
/// `2023-06-01` is the only released revision as of 2026-05 and what the
/// built-in catalog ships in `providers/anthropic.toml`.
pub const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The `anthropic-beta` values the API requires for Claude Pro/Max OAuth
/// credentials. Without these, the upstream rejects requests bearing an
/// `sk-ant-oat…` token with a 401.
///
/// - `oauth-2025-04-20` — opt into the OAuth-credential code path.
/// - `claude-code-20250219` — opt into the Claude Code agent profile,
///   which is what the subscription endpoint admits agents under (see
///   the OpenClaw / OpenCode reference implementations, and the
///   `anthropic-transport-stream.ts` header builder in OpenClaw at
///   <https://github.com/openclaw/openclaw>).
pub const OAUTH_BETA_VALUES: &[&str] = &["claude-code-20250219", "oauth-2025-04-20"];

/// The current Claude Agent SDK identity block accepted by the Claude Code
/// subscription endpoint.
///
/// A standard Anthropic Messages client does not send this block. The
/// `claude-code` subscription applier prepends it while preserving the
/// client's own system instructions, making an explicit subscription route
/// wire-compatible with the first-party agent profile.
pub const CLAUDE_AGENT_SYSTEM_PROMPT: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

/// The legacy Claude Code identity retained for idempotency with older clients.
pub const LEGACY_CLAUDE_CODE_SYSTEM_PROMPT: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// The `user-agent` Claude Code sends. The OAuth path mirrors it so the
/// subscription endpoint sees a first-party-CLI-shaped request.
pub const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.75";

/// The `x-app` value Claude Code sends on OAuth requests.
pub const CLAUDE_CODE_X_APP: &str = "cli";
