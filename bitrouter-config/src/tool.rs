//! Tool server pricing configuration.

/// Pricing for tool server invocations.
///
/// Each tool call has a flat per-invocation cost. Individual tools can
/// override the default rate. This is a type alias for
/// [`bitrouter_core::pricing::FlatPricing`].
pub type ToolPricing = bitrouter_core::pricing::FlatPricing;

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn cost_for_with_default() {
        let pricing = ToolPricing {
            default: 0.001,
            overrides: HashMap::new(),
        };
        assert!((pricing.cost_for("anything") - 0.001).abs() < 1e-10);
    }

    #[test]
    fn cost_for_with_override() {
        let pricing = ToolPricing {
            default: 0.001,
            overrides: HashMap::from([("search".into(), 0.005)]),
        };
        assert!((pricing.cost_for("search") - 0.005).abs() < 1e-10);
        assert!((pricing.cost_for("other") - 0.001).abs() < 1e-10);
    }

    #[test]
    fn cost_for_zero_default() {
        let pricing = ToolPricing::default();
        assert_eq!(pricing.cost_for("anything"), 0.0);
    }

    #[test]
    fn serde_round_trip() {
        let pricing = ToolPricing {
            default: 0.002,
            overrides: HashMap::from([("expensive_tool".into(), 0.05)]),
        };
        let yaml = serde_saphyr::to_string(&pricing).expect("serialize");
        let parsed: ToolPricing = serde_saphyr::from_str(&yaml).expect("deserialize");
        assert!((parsed.default - 0.002).abs() < 1e-10);
        assert!((parsed.cost_for("expensive_tool") - 0.05).abs() < 1e-10);
    }

    #[test]
    fn deserializes_legacy_field_names() {
        let yaml = "default_cost_per_call: 0.003\ntools:\n  search: 0.01\n";
        let parsed: ToolPricing = serde_saphyr::from_str(yaml).expect("deserialize legacy");
        assert!((parsed.default - 0.003).abs() < 1e-10);
        assert!((parsed.cost_for("search") - 0.01).abs() < 1e-10);
    }
}
