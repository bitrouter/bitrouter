//! Tests of private application assembly and its real transports.

#[path = "model_support/conversion.rs"]
mod conversion;
#[path = "../../tests/support/decision.rs"]
mod decision;
#[cfg(any(unix, feature = "mcp"))]
#[path = "../../tests/support/example_binary.rs"]
mod example_binary;
#[cfg(any(unix, feature = "mcp"))]
#[path = "../../tests/support/prebuilt_example.rs"]
mod prebuilt_example;

mod artifact_build_metadata;
mod artifact_profiles;
mod cloud_api;
mod configuration_state;
mod daemon;
mod database_backends;
mod e2e;
mod eval_exchange_equivalence;
mod extension_host;
mod feature_ingress;
mod full_stack;
mod gateway_anthropic_authority;
mod gateway_decision_model;
mod gateway_decisions;
mod gateway_model_boundary;
mod gateway_routing_assessment;
#[cfg(feature = "mcp")]
mod harness_mcp_check;
mod host_lifecycle;
mod http2_ingress;
mod mcp_skills_gateway;
mod model_admitted_conversions;
mod model_anthropic;
mod model_auth;
mod model_content_boundaries;
mod model_decisions;
mod model_explicit_login;
mod model_file_store;
mod model_google_chat;
mod model_hosted;
mod model_hosted_login;
mod model_http;
mod model_ingress_admission;
mod model_invocation;
mod model_projection_admission;
mod model_projection_attributes;
mod model_providers;
mod model_responses_reasoning;
mod model_retirement;
#[cfg(feature = "otel-http")]
mod observe_hierarchy;
mod onboarding;
mod onboarding_pty;
mod policy_cli;
mod policy_eval_control_plane;
mod provider_check;
mod registry_lifecycle;
mod request_checks;
mod router_continuation;
mod semantic_routing;
mod subscription;
mod telemetry_disabled;
#[cfg(feature = "otel-grpc")]
mod telemetry_grpc_transport;
#[cfg(feature = "otel-http")]
mod telemetry_ingress_log_target;
mod tenancy_cli;

/// Process cases consume the candidate binary built by the test runner.
/// They never launch a nested Cargo build.
fn binary() -> anyhow::Result<std::path::PathBuf> {
    let path = std::env::var_os("BITROUTER_TEST_GATEWAY_BINARY")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            anyhow::anyhow!("set BITROUTER_TEST_GATEWAY_BINARY to the prebuilt bitrouter binary")
        })?;
    anyhow::ensure!(
        path.is_absolute() && path.is_file(),
        "BITROUTER_TEST_GATEWAY_BINARY must name an existing absolute binary path"
    );
    Ok(path)
}
