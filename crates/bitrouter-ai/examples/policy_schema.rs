//! Emit the public model-policy schema without building the service.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema = schemars::schema_for!(bitrouter_ai::policy::Document);
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(())
}
