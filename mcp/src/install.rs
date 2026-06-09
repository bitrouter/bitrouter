//! Render (and optionally write) MCP client config blocks for `bitrouter mcp serve`.

/// Supported clients.
#[derive(Debug, Clone, Copy)]
pub enum Client {
    Claude,
    Cursor,
}

/// Render the `mcpServers` entry (stdio) as pretty JSON for `client`.
pub fn render_block(_client: Client) -> serde_json::Value {
    serde_json::json!({
        "mcpServers": {
            "bitrouter": { "command": "bitrouter", "args": ["mcp", "serve"] }
        }
    })
}

/// Merge `block`'s `mcpServers` into an existing client config `doc`
/// non-destructively (never clobbering unrelated servers).
pub fn merge_into(doc: &mut serde_json::Value, block: &serde_json::Value) {
    if !doc.is_object() {
        *doc = serde_json::json!({});
    }
    let Some(dst) = doc.as_object_mut() else {
        return;
    };
    let servers = dst
        .entry("mcpServers")
        .or_insert_with(|| serde_json::json!({}));
    if let (Some(servers), Some(add)) = (servers.as_object_mut(), block["mcpServers"].as_object()) {
        for (k, v) in add {
            servers.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_preserves_existing_servers() {
        let mut existing = serde_json::json!({
            "mcpServers": { "other": { "command": "x" } },
            "theme": "dark"
        });
        merge_into(&mut existing, &render_block(Client::Claude));
        assert_eq!(existing["theme"], "dark");
        assert_eq!(existing["mcpServers"]["other"]["command"], "x");
        assert_eq!(existing["mcpServers"]["bitrouter"]["command"], "bitrouter");
    }
}
