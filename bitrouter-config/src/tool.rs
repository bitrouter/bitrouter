//! Tool server pricing configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Pricing for tool server invocations.
///
/// Each tool call has a flat per-invocation cost. Individual tools can
/// override the default rate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolPricing {
    /// Default cost per tool invocation (USD).
    #[serde(default)]
    pub default_cost_per_call: f64,
    /// Per-tool cost overrides. Keys are un-namespaced tool names.
    #[serde(default)]
    pub tools: HashMap<String, f64>,
}

impl ToolPricing {
    /// Return the cost for a given tool, falling back to the default.
    pub fn cost_for(&self, tool_name: &str) -> f64 {
        self.tools
            .get(tool_name)
            .copied()
            .unwrap_or(self.default_cost_per_call)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_for_with_default() {
        let pricing = ToolPricing {
            default_cost_per_call: 0.001,
            tools: HashMap::new(),
        };
        assert!((pricing.cost_for("anything") - 0.001).abs() < 1e-10);
    }

    #[test]
    fn cost_for_with_override() {
        let pricing = ToolPricing {
            default_cost_per_call: 0.001,
            tools: HashMap::from([("search".into(), 0.005)]),
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
            default_cost_per_call: 0.002,
            tools: HashMap::from([("expensive_tool".into(), 0.05)]),
        };
        let yaml = serde_yaml::to_string(&pricing).expect("serialize");
        let parsed: ToolPricing = serde_yaml::from_str(&yaml).expect("deserialize");
        assert!((parsed.default_cost_per_call - 0.002).abs() < 1e-10);
        assert!((parsed.cost_for("expensive_tool") - 0.05).abs() < 1e-10);
    }
}
