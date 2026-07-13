//! Verifies `install` merges into an on-disk config without clobbering.

#[test]
fn install_writes_and_merges_existing_file() {
    let dir = std::env::temp_dir().join(format!("brmcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("config.json");
    std::fs::write(&path, r#"{"mcpServers":{"keep":{"command":"y"}},"k":1}"#).expect("seed");

    bitrouter_mcp::install(bitrouter_mcp::InstallOptions {
        client: bitrouter_mcp::install::Client::Cursor,
        config_path: Some(path.clone()),
    })
    .expect("install");

    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
    assert_eq!(doc["k"], 1);
    assert_eq!(doc["mcpServers"]["keep"]["command"], "y");
    assert_eq!(doc["mcpServers"]["bitrouter"]["command"], "bitrouter");
    let _ = std::fs::remove_dir_all(&dir);
}
