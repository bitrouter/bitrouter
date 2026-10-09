//! File-backend/application boundary coverage for AI subscription integrations.
#[path = "subscription/codex.rs"]
mod codex;
#[path = "subscription/copilot.rs"]
mod copilot;
#[path = "subscription/supergrok.rs"]
mod supergrok;

#[path = "subscription/anthropic.rs"]
mod anthropic;
#[path = "subscription/claude_code.rs"]
mod claude_code;

#[path = "subscription/hosted.rs"]
mod hosted;
