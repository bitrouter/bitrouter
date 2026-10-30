//! Authenticated canonical turns through the existing controlled pipeline.
//!
//! The embedding control owns its callbacks, not tenant authority. No host
//! transform, credential plugin, cost reader or session context is inherited.

use std::{io::Write, sync::Arc};

use bitrouter_ai::{tenancy::operation::ErrorCode, types::Prompt};
use http::HeaderMap;

use super::Service;
mod control;
use crate::{
    gateway::{
        error::{BitrouterError, Result},
        pipeline::{
            native::NativeExecutionControl,
            types::{PipelineRequest, PipelineResponse},
        },
    },
    tenancy::error::Failure,
};

const PROMPT_BYTES: usize = 16 * 1024 * 1024;
const HEADER_BYTES: usize = 64 * 1024;
const HEADER_VALUES: usize = 128;

impl Service {
    pub(crate) async fn execute_native_controlled(
        &self,
        prompt: Prompt,
        headers: HeaderMap,
        control: Arc<dyn NativeExecutionControl>,
    ) -> Result<PipelineResponse> {
        if prompt.stream {
            return Err(BitrouterError::bad_request(
                "controlled model turns must be non-streaming",
            ));
        }
        bounded_headers(&headers).map_err(native_failure)?;
        let submission = self.submission(&headers).map_err(native_failure)?;
        bounded_prompt(&prompt).map_err(native_failure)?;
        let execution = self
            .admit_native_execution(submission, &prompt.model)
            .await
            .map_err(native_failure)?;
        self.execute_admitted_native(
            prompt,
            headers,
            control,
            execution,
            #[cfg(all(feature = "core", feature = "harness"))]
            None,
        )
        .await
    }

    #[cfg(all(feature = "core", feature = "harness"))]
    pub(crate) async fn execute_session_native(
        &self,
        step: &super::session::StepBinding,
        prompt: Prompt,
        headers: HeaderMap,
        control: Arc<dyn NativeExecutionControl>,
    ) -> Result<PipelineResponse> {
        let binding = step.binding();
        if !binding.belongs_to(self) {
            return Err(native_failure(ErrorCode::CapabilityMismatch.into()));
        }
        if prompt.stream {
            return Err(BitrouterError::bad_request(
                "controlled model turns must be non-streaming",
            ));
        }
        bounded_prompt(&prompt).map_err(native_failure)?;
        bounded_headers(&headers).map_err(native_failure)?;
        // Owned Core constructors start with empty headers; the step adds only
        // committed run/agent associations. These are trace inputs, never scope,
        // authentication or a replacement for the binding's retained revision.
        if headers.keys().any(|name| {
            !matches!(
                name.as_str(),
                "x-bitrouter-parent-session-id" | "x-bitrouter-agent-session-id"
            )
        }) {
            return Err(native_failure(ErrorCode::InvalidRequest.into()));
        }
        let execution = binding.admit(&prompt.model).await.map_err(native_failure)?;
        self.execute_admitted_native(prompt, headers, control, execution, Some(step))
            .await
    }

    pub(in crate::tenancy) async fn execute_admitted_native(
        &self,
        prompt: Prompt,
        headers: HeaderMap,
        control: Arc<dyn NativeExecutionControl>,
        execution: Arc<super::TenantExecution>,
        #[cfg(all(feature = "core", feature = "harness"))] step: Option<
            &super::session::StepBinding,
        >,
    ) -> Result<PipelineResponse> {
        let outcome = async {
            #[cfg(all(feature = "core", feature = "harness"))]
            if let Some(step) = step {
                execution.enroll_experiment(step).await?;
            }
            if let Err(code) = execution.prepare_count_profile(&prompt) {
                let error = execution.reject(code);
                execution
                    .terminal_receipt(bitrouter_ai::tenancy::execution::Outcome::Failed)
                    .await?;
                return Err(error);
            }
            let pipeline = execution.pipeline()?;
            let mut request =
                PipelineRequest::new(prompt.model.clone(), execution.caller(), prompt);
            request.request_id = execution.id().to_string();
            request.original_model = request.model.clone();
            request.headers = headers;
            // Canonical input has no HTTP wire protocol preference. Its private
            // retained selection mode cannot be supplied through public headers.
            request.inbound_protocol = None;
            execution.attach(pipeline.clone(), &mut request)?;
            let tasks = pipeline
                .owned_tasks()
                .cloned()
                .ok_or(BitrouterError::UpstreamUnavailable)?;
            #[cfg(all(feature = "core", feature = "harness"))]
            if let Some(step) = step {
                step.attach_parent(&execution, tasks.clone())
                    .map_err(native_failure)?;
            }
            pipeline
                .execute_native_controlled(
                    request,
                    Arc::new(control::Bounded::new(execution.clone(), control, tasks)),
                )
                .await
        }
        .await;
        // The autonomous receipt watchdog can close the output handoff before
        // the callback's equal-deadline timeout gets polled. Preserve the native
        // capacity classification in either ordering. Earlier authority failures
        // remain sticky; receipt storage failure still has priority in failure().
        if outcome.is_err()
            && execution.remaining().is_zero()
            && execution.receipt_owner()?.cancelled.is_cancelled()
        {
            execution.reject(ErrorCode::CapacityExceeded);
        }
        match execution.failure() {
            Some(failure) => Err(native_failure(failure)),
            None => outcome,
        }
    }
}

/// Preserve the existing native error API. The full tenancy error/dispatch
/// state remains available through HTTP; it is never encoded into diagnostics.
fn native_failure(failure: Failure) -> BitrouterError {
    let message = failure.0.message;
    match failure.0.code {
        ErrorCode::InvalidRequest | ErrorCode::PayloadTooLarge => {
            BitrouterError::bad_request(message)
        }
        ErrorCode::Unauthorized => BitrouterError::Unauthorized(message),
        ErrorCode::Forbidden
        | ErrorCode::TenantInactive
        | ErrorCode::RevisionConflict
        | ErrorCode::OperationConflict
        | ErrorCode::CapabilityMismatch => BitrouterError::Forbidden(message),
        ErrorCode::NotFound | ErrorCode::OperationExpired => BitrouterError::NotFound(message),
        ErrorCode::CapacityExceeded => BitrouterError::RateLimited { retry_after: None },
        ErrorCode::AuthorityUnavailable | ErrorCode::StorageUnavailable => {
            BitrouterError::UpstreamUnavailable
        }
    }
}

pub(super) fn bounded_headers(headers: &HeaderMap) -> std::result::Result<(), Failure> {
    let mut bytes = 0_usize;
    let mut count = 0_usize;
    for (name, value) in headers {
        count = count
            .checked_add(1)
            .filter(|count| *count <= HEADER_VALUES)
            .ok_or(ErrorCode::PayloadTooLarge)?;
        bytes = bytes
            .checked_add(name.as_str().len())
            .and_then(|bytes| bytes.checked_add(value.as_bytes().len()))
            .filter(|bytes| *bytes <= HEADER_BYTES)
            .ok_or(ErrorCode::PayloadTooLarge)?;
    }
    Ok(())
}

fn bounded_prompt(prompt: &Prompt) -> std::result::Result<(), Failure> {
    // The caller already owns a typed Prompt. Count its actual encoded bytes
    // without constructing another large buffer before native admission.
    ByteLimit::new().value(prompt)
}

struct ByteLimit {
    remaining: usize,
    exceeded: bool,
}

impl ByteLimit {
    fn new() -> Self {
        Self {
            remaining: PROMPT_BYTES,
            exceeded: false,
        }
    }

    fn value(&mut self, value: &impl serde::Serialize) -> std::result::Result<(), Failure> {
        serde_json::to_writer(&mut *self, value).map_err(|_| {
            Failure::from(if self.exceeded {
                ErrorCode::PayloadTooLarge
            } else {
                ErrorCode::InvalidRequest
            })
        })
    }
}

impl Write for ByteLimit {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self.remaining.checked_sub(bytes.len()) {
            Some(remaining) => {
                self.remaining = remaining;
                Ok(bytes.len())
            }
            None => {
                self.exceeded = true;
                Err(std::io::Error::other(
                    "canonical input byte capacity exceeded",
                ))
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
