//! Independent JSON-RPC peer for Skills gateway/host protocol tests.
//! Configuration can change between connections without restarting a test.

use std::error::Error;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let [configuration, journal] = arguments.as_slice() else {
        return Err("expected configuration and request journal paths".into());
    };
    let mut journal = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(journal)?;
    let mut output = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        writeln!(journal, "{request}")?;
        journal.flush()?;
        let Some(id) = request.get("id") else {
            continue;
        };
        let config: Value = serde_json::from_slice(&std::fs::read(configuration)?)?;
        let capabilities = config
            .get("capabilities")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let method = request["method"].as_str().ok_or("missing method")?;
        let result = match method {
            "server/discover" => Some(json!({
                "resultType":"complete","supportedVersions":["2026-07-28"],
                "capabilities":capabilities,"ttlMs":0,"cacheScope":"private",
                "_meta":{"io.modelcontextprotocol/serverInfo":{"name":"untrusted-self-label","version":"1"}}
            })),
            "initialize" => Some(json!({
                "protocolVersion":request["params"]["protocolVersion"],
                "capabilities":capabilities,"serverInfo":{"name":"untrusted-self-label","version":"1"}
            })),
            "skills/list" => {
                let cursor = request["params"]["cursor"].as_str().unwrap_or("first");
                config["pages"].get(cursor).cloned()
            }
            "skills/get" => request["params"]["uri"].as_str().and_then(|uri| config["get"].get(uri)).cloned(),
            "resources/read" => request["params"]["uri"].as_str().and_then(|uri| {
                config["files"].get(uri).map(|file| {
                    let mut file = file.clone();
                    file["uri"] = uri.into();
                    json!({"resultType":"complete","contents":[file],"ttlMs":2000,"cacheScope":"private"})
                })
            }),
            "resources/list" => Some(json!({"resultType":"complete","resources":[],"ttlMs":0,"cacheScope":"private"})),
            "resources/directory/read" => Some(json!({"resultType":"complete","resources":[],"ttlMs":0,"cacheScope":"private"})),
            "tools/list" => Some(json!({"resultType":"complete","tools":[],"ttlMs":0,"cacheScope":"private"})),
            _ => None,
        };
        let response = match result {
            Some(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            None => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":if matches!(method,"skills/get"|"resources/read"|"skills/list"){-32602}else{-32601},"message":"fixture unknown URI or method"}})
            }
        };
        writeln!(output, "{response}")?;
        output.flush()?;
        if config.get("closeAfter").and_then(Value::as_str) == Some(method) {
            break;
        }
    }
    Ok(())
}
