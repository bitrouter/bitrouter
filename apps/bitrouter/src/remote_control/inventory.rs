//! Explicit control exposure, independent of the cloud MCP profile.

use bitrouter_ai::control::ControlScope;
use bitrouter_ai::control::action::Action;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub struct ControlActionSpec {
    pub action: Action,
    pub id: &'static str,
    pub version: u32,
    pub path: &'static str,
    pub scope: ControlScope,
    pub cli_leaf: &'static str,
    pub requires_administration: bool,
    pub requires_reload: bool,
    pub input_schema: fn() -> serde_json::Value,
    pub output_schema: fn() -> serde_json::Value,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmptyInput {}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelsInput {
    pub provider: Option<String>,
}

fn input_schema<T: JsonSchema>() -> serde_json::Value {
    schemars::schema_for!(T).to_value()
}

fn output_schema<T: JsonSchema>() -> serde_json::Value {
    let mut schema = schemars::schema_for!(T).to_value();
    if let Some(object) = schema.as_object_mut() {
        object.remove("title");
        object.remove("description");
    }
    schema
}

macro_rules! action {
    ($variant:ident, $path:literal, $cli:literal, $admin:literal, $input:ty, $output:ty) => {
        ControlActionSpec {
            action: Action::$variant,
            id: Action::$variant.id(),
            version: Action::$variant.version(),
            path: concat!("/control/v1", $path),
            scope: Action::$variant.required_scope(),
            cli_leaf: $cli,
            requires_administration: $admin,
            requires_reload: false,
            input_schema: input_schema::<$input>,
            output_schema: output_schema::<$output>,
        }
    };
}

pub const ACTIONS: &[ControlActionSpec] = &[
    ControlActionSpec {
        action: Action::Reload,
        id: Action::Reload.id(),
        version: Action::Reload.version(),
        path: "/control/v1/reload",
        scope: Action::Reload.required_scope(),
        cli_leaf: "reload",
        requires_administration: false,
        requires_reload: true,
        input_schema: input_schema::<bitrouter_ai::control::reload::ReloadInput>,
        output_schema: output_schema::<bitrouter_ai::control::reload::OperationReport>,
    },
    action!(
        Status,
        "/status",
        "status",
        false,
        EmptyInput,
        bitrouter_ai::control::status::StatusReport
    ),
    action!(
        Models,
        "/models",
        "models",
        false,
        ModelsInput,
        bitrouter_ai::control::models::ModelsReport
    ),
    action!(
        Route,
        "/route/preview",
        "route",
        false,
        bitrouter_ai::control::route::RouteInput,
        bitrouter_ai::control::route::RouteReport
    ),
    action!(
        Requests,
        "/requests",
        "requests",
        false,
        bitrouter_ai::control::requests::RequestFilters,
        bitrouter_ai::control::requests::RequestsReport
    ),
    action!(
        Providers,
        "/providers",
        "providers list",
        true,
        EmptyInput,
        bitrouter_ai::control::administration::ProvidersReport
    ),
    action!(
        Observe,
        "/observe/status",
        "observe status",
        true,
        EmptyInput,
        bitrouter_ai::control::administration::ObserveReport
    ),
    action!(
        PolicyStatus,
        "/policy/status",
        "policy status",
        true,
        bitrouter_ai::control::administration::PolicyInput,
        bitrouter_ai::control::administration::PolicyReport
    ),
    action!(
        PolicyShow,
        "/policy/show",
        "policy show",
        true,
        bitrouter_ai::control::administration::PolicyInput,
        bitrouter_ai::control::administration::PolicyReport
    ),
    action!(
        Agents,
        "/agents",
        "agents list",
        true,
        EmptyInput,
        bitrouter_ai::control::administration::AgentsReport
    ),
];

pub fn by_cli(leaf: &str) -> Option<&'static ControlActionSpec> {
    ACTIONS.iter().find(|row| row.cli_leaf == leaf)
}

/// Resources have separate identities from executable actions.
pub struct ControlResourceSpec {
    pub id: &'static str,
    pub path: &'static str,
    pub scope: ControlScope,
    pub cli_leaf: Option<&'static str>,
    pub requires_reload: bool,
}
pub const RESOURCES: &[ControlResourceSpec] = &[
    ControlResourceSpec {
        id: "capabilities",
        path: "/control/v1/capabilities",
        scope: ControlScope::Read,
        cli_leaf: None,
        requires_reload: false,
    },
    ControlResourceSpec {
        id: "state",
        path: "/control/v1/state",
        scope: ControlScope::Read,
        cli_leaf: None,
        requires_reload: true,
    },
    ControlResourceSpec {
        id: "operation",
        path: "/control/v1/operations/{request_id}",
        scope: ControlScope::Reload,
        cli_leaf: Some("operations show"),
        requires_reload: true,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn exposure_has_unique_paths_and_valid_schemas() {
        let mut paths = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for row in ACTIONS {
            assert!(paths.insert(row.path));
            assert!(ids.insert(row.id));
            assert!((row.input_schema)().is_object());
            assert!((row.output_schema)().is_object());
        }
    }
}
