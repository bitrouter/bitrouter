//! Explicit control exposure, independent of the cloud MCP profile.

use bitrouter_sdk::config::ControlScope;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::actions::administration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    Models,
    Route,
    Requests,
    Providers,
    Observe,
    PolicyStatus,
    PolicyShow,
    Agents,
    Reload,
}

pub struct ControlActionSpec {
    pub action: Action,
    pub id: &'static str,
    pub legacy_name: &'static str,
    pub shared_id: Option<&'static str>,
    pub version: u32,
    pub method: &'static str,
    pub path: &'static str,
    pub scope: ControlScope,
    pub cli_leaf: &'static str,
    pub dashboard: &'static str,
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

fn output_schema<T: JsonSchema + 'static>() -> serde_json::Value {
    serde_json::Value::Object(
        rmcp::handler::server::tool::schema_for_output::<T>()
            .as_ref()
            .clone(),
    )
}

macro_rules! action {
    ($variant:ident, $id:literal, $legacy:literal, $shared:expr, $method:literal, $path:literal, $cli:literal, $panel:literal, $admin:literal, $input:ty, $output:ty) => {
        ControlActionSpec {
            action: Action::$variant,
            id: $id,
            legacy_name: $legacy,
            shared_id: $shared,
            version: 1,
            method: $method,
            path: concat!("/control/v1", $path),
            scope: ControlScope::Read,
            cli_leaf: $cli,
            dashboard: $panel,
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
        id: "reload",
        legacy_name: "reload",
        shared_id: None,
        version: 1,
        method: "POST",
        path: "/control/v1/reload",
        scope: ControlScope::Reload,
        cli_leaf: "reload",
        dashboard: "reload",
        requires_administration: false,
        requires_reload: true,
        input_schema: input_schema::<super::operations::ReloadInput>,
        output_schema: output_schema::<super::operations::OperationReport>,
    },
    action!(
        Status,
        "status",
        "status",
        Some("status"),
        "GET",
        "/status",
        "status",
        "overview",
        false,
        EmptyInput,
        bitrouter_mcp::actions::status::StatusReport
    ),
    action!(
        Models,
        "list_models",
        "models",
        Some("list_models"),
        "GET",
        "/models",
        "models",
        "models",
        false,
        ModelsInput,
        bitrouter_mcp::actions::models::ModelsReport
    ),
    action!(
        Route,
        "route",
        "route_preview",
        Some("route"),
        "POST",
        "/route/preview",
        "route",
        "preview",
        false,
        bitrouter_mcp::actions::route::RouteInput,
        bitrouter_mcp::actions::route::RouteReport
    ),
    action!(
        Requests,
        "requests",
        "requests",
        None,
        "GET",
        "/requests",
        "requests",
        "requests",
        false,
        crate::actions::requests::RequestFilters,
        crate::output::reports::requests::RequestsReport
    ),
    action!(
        Providers,
        "providers_list",
        "providers_list",
        None,
        "GET",
        "/providers",
        "providers list",
        "providers",
        true,
        EmptyInput,
        administration::ProvidersReport
    ),
    action!(
        Observe,
        "observe_status",
        "observe_status",
        None,
        "GET",
        "/observe/status",
        "observe status",
        "telemetry",
        true,
        EmptyInput,
        administration::ObserveReport
    ),
    action!(
        PolicyStatus,
        "policy_status",
        "policy_status",
        None,
        "GET",
        "/policy/status",
        "policy status",
        "policy",
        true,
        administration::PolicyInput,
        administration::PolicyReport
    ),
    action!(
        PolicyShow,
        "policy_show",
        "policy_show",
        None,
        "GET",
        "/policy/show",
        "policy show",
        "policy",
        true,
        administration::PolicyInput,
        administration::PolicyReport
    ),
    action!(
        Agents,
        "agents_list",
        "agents_list",
        None,
        "GET",
        "/agents",
        "agents list",
        "agents",
        true,
        EmptyInput,
        administration::AgentsReport
    ),
];

pub fn by_id(id: &str) -> Option<&'static ControlActionSpec> {
    ACTIONS.iter().find(|row| row.id == id)
}

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
    fn exposure_reuses_shared_identity_and_report_schema() -> anyhow::Result<()> {
        let mut paths = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for row in ACTIONS {
            assert!(paths.insert((row.method, row.path)));
            assert!(ids.insert(row.id));
            assert!((row.input_schema)().is_object());
            assert!((row.output_schema)().is_object());
            if let Some(shared_id) = row.shared_id {
                let shared = bitrouter_mcp::actions::ACTIONS
                    .iter()
                    .find(|shared| shared.id == shared_id)
                    .ok_or_else(|| anyhow::anyhow!("missing shared action"))?;
                assert_eq!(row.id, shared.id);
                let schema = shared
                    .output_schema
                    .ok_or_else(|| anyhow::anyhow!("missing shared schema"))?;
                assert_eq!((row.output_schema)(), serde_json::Value::Object(schema()));
            }
        }
        Ok(())
    }
}
