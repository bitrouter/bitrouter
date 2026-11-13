//! Emit the standalone connector configuration schema for contributor tooling.

fn main() -> anyhow::Result<()> {
    let schema = schemars::schema_for!(bitrouter::connector::config::ConnectorConfig);
    serde_json::to_writer_pretty(std::io::stdout(), &schema)?;
    println!();
    Ok(())
}
