//! HTTP headers Copilot requires on every chat / completions request.
//!
//! The Copilot REST surface gates editor-integration calls behind a small
//! set of headers (`Editor-Version`, `Copilot-Integration-Id`, …). Requests
//! without them are rejected with 400/403 even when the Bearer is valid.
//!
//! References:
//! - VS Code Copilot Chat sets the same headers on every chat call:
//!   <https://github.com/microsoft/vscode-copilot-chat>
//! - opencode does the same in `src/auth/copilot.ts`:
//!   <https://github.com/sst/opencode/blob/dev/packages/opencode/src/auth/copilot.ts>
//!
//! The `Copilot-Integration-Id` value is the integration GitHub recognises
//! for our calls — `vscode-chat` is the historically accepted value for
//! third-party clients reusing the editor-style chat surface; using it lets
//! Copilot recognise the traffic without us having a private integration
//! registration.

/// `Editor-Version` value sent on every request. Format is
/// `<editor>/<version>`; the Copilot API logs use this for client
/// attribution.
pub const EDITOR_VERSION_HEADER_VALUE: &str = concat!("bitrouter/", env!("CARGO_PKG_VERSION"),);

/// `Copilot-Integration-Id` value. `vscode-chat` is the well-known id the
/// editor uses; mirroring it keeps Copilot's request routing happy.
pub const COPILOT_INTEGRATION_ID: &str = "vscode-chat";

/// Build the (header_name, header_value) pairs that go on every Copilot
/// chat / completions / messages request.
pub fn copilot_request_headers() -> Vec<(String, String)> {
    vec![
        (
            "Editor-Version".to_string(),
            EDITOR_VERSION_HEADER_VALUE.to_string(),
        ),
        (
            "Editor-Plugin-Version".to_string(),
            EDITOR_VERSION_HEADER_VALUE.to_string(),
        ),
        (
            "Copilot-Integration-Id".to_string(),
            COPILOT_INTEGRATION_ID.to_string(),
        ),
        // `Openai-Intent` mirrors the v0 implementation; the Copilot API
        // logs this for usage analytics. `conversation-edits` is the broad
        // chat intent.
        (
            "Openai-Intent".to_string(),
            "conversation-edits".to_string(),
        ),
        (
            "User-Agent".to_string(),
            format!("bitrouter/{}", env!("CARGO_PKG_VERSION")),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_includes_integration_id_and_editor_version() {
        let h = copilot_request_headers();
        let names: Vec<&str> = h.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"Copilot-Integration-Id"));
        assert!(names.contains(&"Editor-Version"));
    }

    #[test]
    fn editor_version_format() {
        assert!(EDITOR_VERSION_HEADER_VALUE.starts_with("bitrouter/"));
    }
}
