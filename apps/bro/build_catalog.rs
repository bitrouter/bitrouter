//! Compile BRO's package-local ACP registry snapshots into its launch catalog.
use serde_json::Value;
use std::error::Error;
use std::fmt::Write;
use std::path::Path;

pub fn generate(manifest: &Path, dest: &Path) -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-changed=build_catalog.rs");
    let load = |name: &str| -> Result<Vec<Value>, Box<dyn Error>> {
        let path = manifest.join("registry-dist").join(name);
        println!("cargo::rerun-if-changed={}", path.display());
        let document: Value = serde_json::from_slice(&std::fs::read(path)?)?;
        document["data"]
            .as_array()
            .cloned()
            .ok_or_else(|| format!("{name} has no data array").into())
    };
    let agents = load("agents.json")?;
    let runtimes = load("runtimes.json")?;
    let local = runtimes
        .iter()
        .find(|r| r["id"] == "local")
        .ok_or("missing local ACP runtime")?;
    let entries = local["agents"]
        .as_array()
        .ok_or("local runtime has no agents")?;
    let mut catalog = String::from("pub(crate) const CATALOG: &[Harness] = &[\n");
    for agent in agents {
        let id = field(&agent, "id")?;
        let invocation = agent["runtimes"]
            .as_array()
            .and_then(|rs| rs.iter().find(|r| r["runtime"] == "local"))
            .ok_or_else(|| format!("agent {id} has no local invocation"))?;
        let transport = &invocation["transport"];
        let entry = entries
            .iter()
            .find(|a| a["id"] == id)
            .ok_or_else(|| format!("{id} absent from local runtime"))?;
        if entry["transport"] != *transport
            || transport["type"] != "stdio"
            || agent["acp"]["protocol_version"] != 1
        {
            return Err(format!("inconsistent or unsupported local ACP transport for {id}").into());
        }
        let args = transport["args"]
            .as_array()
            .ok_or("expected an argument array")?
            .iter()
            .map(|v| v.as_str().ok_or("expected a string argument"))
            .collect::<Result<Vec<_>, _>>()?;
        let description = field(&agent, "description")?;
        let command = field(transport, "command")?;
        let routing = &agent["routing"];
        // The provider RPC/fallback contracts were verified against these exact adapter releases.
        // A new package version stays discoverable pending review of its endpoint contract.
        let route = match (id, field(routing, "kind")?, command, args.as_slice()) {
            (
                "claude-acp",
                "env",
                "npx",
                ["-y", "@agentclientprotocol/claude-agent-acp@0.75.1"],
            ) if routing["bearer_auth"] == true
                && routing["base_url_env"] == "ANTHROPIC_BASE_URL"
                && routing["auth_env"] == "ANTHROPIC_AUTH_TOKEN"
                && routing["model_env"] == "ANTHROPIC_MODEL"
                && routing.get("extra").is_none() =>
            {
                "Some(Adapter::Claude)"
            }
            ("codex-acp", "codex_args", "npx", ["-y", "@agentclientprotocol/codex-acp@1.10.0"]) => {
                "Some(Adapter::Codex)"
            }
            _ => "None",
        };
        writeln!(
            catalog,
            "Harness {{ id: {id:?}, description: {description:?}, command: {command:?}, args: &{args:?}, adapter: {route} }},"
        )?;
    }
    catalog.push_str("];\n");
    std::fs::write(dest.join("catalog_generated.rs"), catalog)?;
    Ok(())
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a str, Box<dyn Error>> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("expected a registry string at {key}").into())
}
