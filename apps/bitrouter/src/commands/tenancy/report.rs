//! CLI projections and redacted recovery diagnostics for the HTTP adapter.

use bitrouter::gateway::error::BitrouterError;
use bitrouter::output::{CliReport, human::Human};
use bitrouter_ai::tenancy::{
    configuration::{ActiveRevision, ConfigurationRevision},
    identity::{InstallationId, OperationId},
    operation::{ErrorCode, OperationResult, OperationStatus, TenantCapabilities},
};
use bitrouter_sdk::error::Error;
use serde::Serialize;

pub(super) struct Completion {
    pub(super) report: Report,
    pub(super) recovery: Option<Recovery>,
}

impl Completion {
    pub(super) fn read(report: Report) -> Self {
        Self {
            report,
            recovery: None,
        }
    }

    pub(super) fn mutation(report: Report, recovery: Recovery) -> Self {
        Self {
            report,
            recovery: Some(recovery),
        }
    }
}

pub(super) struct Recovery {
    installation: InstallationId,
    operation: OperationId,
}

impl Recovery {
    pub(super) fn new(installation: &InstallationId, operation: &OperationId) -> Self {
        Self {
            installation: installation.clone(),
            operation: operation.clone(),
        }
    }

    pub(super) fn hint(&self) -> String {
        format!(
            "query `bro tenancy operation --installation-id {} --operation-id {}` with the same --context or --endpoint/--token-env target and current read authority; keep the original request and do not retry with a new operation ID",
            self.installation, self.operation
        )
    }
}

#[derive(Serialize)]
pub(super) struct LocalValidation {
    valid: bool,
    validation: &'static str,
    contract: &'static str,
    server_checked: bool,
}

#[derive(Serialize)]
#[serde(untagged)]
pub(super) enum Report {
    Validated(LocalValidation),
    Capabilities(TenantCapabilities),
    Configuration(Box<ConfigurationRevision>),
    Active(ActiveRevision),
    Operation(OperationResult),
}

impl Report {
    pub(super) fn validated() -> Self {
        Self::Validated(LocalValidation {
            valid: true,
            validation: "local_shape_only",
            contract: "ConfigurationPut",
            server_checked: false,
        })
    }
}

impl CliReport for Report {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        match self {
            Self::Validated(_) => {
                human.line("ConfigurationPut shape and bounds are valid (local only).")?;
                human.note("No server authorization, resource lookup, compilation, CAS, or dispatch checks were performed.")
            }
            Self::Capabilities(caps) => {
                human.embedded_json(&serde_json::to_value(caps).map_err(std::io::Error::other)?)?;
                if !(caps.revision_bound_dispatch && caps.strict_receipts && caps.dispatch_enabled)
                {
                    human.note("Strict tenant dispatch is unavailable; configuration administration does not enable inference.")?;
                }
                Ok(())
            }
            Self::Configuration(_) | Self::Active(_) => {
                human.embedded_json(&serde_json::to_value(self).map_err(std::io::Error::other)?)
            }
            Self::Operation(operation) => {
                human.embedded_json(
                    &serde_json::to_value(operation).map_err(std::io::Error::other)?,
                )?;
                if operation.status != OperationStatus::Applied {
                    human.note("This operation is not confirmed applied. Retain its original ID and request; an unknown result does not prove rollback.")?;
                }
                Ok(())
            }
        }
    }

    fn exit_code(&self) -> i32 {
        match self {
            Self::Operation(operation) if operation.status != OperationStatus::Applied => 1,
            _ => 0,
        }
    }
}

pub(super) fn client_error(error: Error, recovery: Option<&Recovery>) -> anyhow::Error {
    let classified = match error {
        Error::TenancyUnknown { operation_id, .. } => BitrouterError::Upstream {
            status: 502,
            message: recovery.map_or_else(
                || format!("tenancy mutation outcome is unknown; query operation {operation_id} under its original installation and do not retry with a new operation ID"),
                |recovery| format!("tenancy mutation outcome is unknown; {}", recovery.hint()),
            ),
        },
        Error::Tenancy(failure) => {
            let Some(body) = failure.body else {
                return BitrouterError::UpstreamInvalidResponse {
                    message: "invalid tenancy service response".into(), usage: None,
                }.into();
            };
            // Keep the stable code, never the arbitrary remote message, headers,
            // transport source or malformed response bytes in CLI diagnostics.
            let message = format!("tenancy request rejected ({:?})", body.code);
            match body.code {
                ErrorCode::Unauthorized => BitrouterError::Unauthorized(message),
                ErrorCode::Forbidden | ErrorCode::TenantInactive => BitrouterError::Forbidden(message),
                ErrorCode::NotFound => BitrouterError::NotFound(message),
                ErrorCode::InvalidRequest | ErrorCode::PayloadTooLarge => BitrouterError::bad_request(message),
                _ => BitrouterError::Upstream { status: failure.status, message },
            }
        }
        Error::Options(message) | Error::Request(message) => BitrouterError::bad_request(message),
        Error::Transport(_) => BitrouterError::UpstreamUnavailable,
        _ => BitrouterError::UpstreamInvalidResponse {
            message: "invalid tenancy service response".into(), usage: None,
        },
    };
    classified.into()
}
