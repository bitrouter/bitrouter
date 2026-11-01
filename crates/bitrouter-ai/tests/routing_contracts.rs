use bitrouter_ai::routing::{RoutingConfig, SortOrder};

#[test]
fn routing_sort_wire_values_and_legacy_default() -> Result<(), Box<dyn std::error::Error>> {
    for (wire, value) in [
        ("alphabetical", SortOrder::Alphabetical),
        ("cost", SortOrder::Cost),
        ("latency", SortOrder::Latency),
        ("throughput", SortOrder::Throughput),
    ] {
        assert_eq!(
            serde_json::from_value::<SortOrder>(serde_json::json!(wire))?,
            value
        );
        assert_eq!(serde_json::to_value(value)?, serde_json::json!(wire));
    }
    let legacy: RoutingConfig = serde_json::from_str("{}")?;
    assert_eq!(legacy.sort, None);
    assert_eq!(SortOrder::default(), SortOrder::Alphabetical);
    Ok(())
}

#[cfg(feature = "schema")]
#[test]
fn routing_sort_schema_accepts_throughput() -> Result<(), Box<dyn std::error::Error>> {
    let schema = serde_json::to_value(schemars::schema_for!(SortOrder))?;
    let serialized = schema.to_string();
    for value in ["alphabetical", "cost", "latency", "throughput"] {
        assert!(serialized.contains(&format!("\"{value}\"")));
    }
    Ok(())
}
