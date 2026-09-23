//! Host-owned evaluation execution, distinct from language-model generation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::caller::CallerContext;
use crate::config::ConfigRoutingTable;
use crate::error::{BitrouterError, Result};
use crate::evaluation::{
    EvaluationQuestion, EvaluationQuestionType, EvaluationRequest, EvaluationResult,
    EvaluationUsage,
};
use crate::extension::ExtensionApi;
use crate::language_model::HttpExecutor;

/// The observed end of one started provider/account attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluationAttemptTerminal {
    /// A complete, validated answer was received.
    Completed,
    /// The attempt failed with a known provider or transport error.
    Failed,
    /// A configured deadline elapsed.
    TimedOut,
    /// The caller cancelled before a provider response was known.
    UnknownRemoteCompletion,
}

/// Content-free evidence for one provider/account attempt.
#[derive(Debug, Clone)]
pub struct EvaluationAttemptRecord {
    /// Stable host request id.
    pub request_id: String,
    /// Requested canonical or pinned selector.
    pub selector: String,
    /// Canonical model id resolved from the requested selector.
    pub canonical_model: String,
    /// Authenticated caller key id, or the explicit local/anonymous sentinel.
    pub caller_api_key_id: String,
    /// Authenticated owning user id, or the explicit local/anonymous sentinel.
    pub caller_user_id: String,
    /// Configured provider id.
    pub provider: String,
    /// Exact provider model id.
    pub provider_model_id: String,
    /// Actual provider-reported version after a complete valid answer.
    pub reported_model: Option<String>,
    /// Non-secret configured account label, if any.
    pub account_label: Option<String>,
    /// One-based index in the eligible same-provider account chain.
    pub attempt: usize,
    /// Format facet selected by configuration.
    pub format: String,
    /// Time spent waiting for this attempt, including HTTP body receipt.
    pub duration_ms: u64,
    /// Observed terminal state; never claims remote cancellation.
    pub terminal: EvaluationAttemptTerminal,
    /// Stable error class without upstream body or evaluation content.
    pub error_code: Option<&'static str>,
    /// Provider-returned token counts only after a complete valid answer.
    pub usage: Option<EvaluationUsage>,
}

/// A host's durable metering and terminal-outcome authority.
///
/// The returned cost is host-settled USD for a completed attempt, or `None`
/// when price/charge evidence is unavailable. Implementations must persist
/// the record before returning successfully.
#[async_trait]
pub trait EvaluationAttemptRecorder: Send + Sync {
    /// Persist one terminal attempt without storing the evaluated content.
    async fn record(&self, record: EvaluationAttemptRecord) -> Result<Option<f64>>;
}

/// Internal typed-evaluation pipeline. It does not mount an HTTP endpoint.
pub struct EvaluationPipeline {
    routing: Arc<ConfigRoutingTable>,
    http: Arc<HttpExecutor>,
    extensions: ExtensionApi,
    recorder: Arc<dyn EvaluationAttemptRecorder>,
    tasks: TaskTracker,
    accepting: Arc<AtomicBool>,
    admission: Mutex<()>,
}

struct CancellationOnDrop(CancellationToken);

struct EvaluationJob {
    route: crate::config::routing_table::EvaluationRoute,
    request: EvaluationRequest,
    request_id: String,
    inbound_headers: http::HeaderMap,
    caller: CallerContext,
    cancellation: CancellationToken,
}

impl Drop for CancellationOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl EvaluationPipeline {
    /// Build the internal rail from an already validated custom-host registry.
    pub fn new(
        routing: Arc<ConfigRoutingTable>,
        http: Arc<HttpExecutor>,
        extensions: ExtensionApi,
        recorder: Arc<dyn EvaluationAttemptRecorder>,
    ) -> Self {
        Self {
            routing,
            http,
            extensions,
            recorder,
            tasks: TaskTracker::new(),
            accepting: Arc::new(AtomicBool::new(true)),
            admission: Mutex::new(()),
        }
    }

    /// Evaluate through one positively declared provider and its account
    /// chain. Dropping the caller future cancels waiting and marks the
    /// in-flight attempt's remote completion as unknown; detached settlement
    /// remains tracked for graceful shutdown.
    pub async fn evaluate(
        &self,
        request: EvaluationRequest,
        request_id: String,
        inbound_headers: http::HeaderMap,
    ) -> Result<EvaluationResult> {
        self.evaluate_with_caller(
            request,
            request_id,
            inbound_headers,
            CallerContext::anonymous(),
        )
        .await
    }

    /// Evaluate after the embedding host has established the inbound caller.
    /// The caller identity is retained only as content-free attempt evidence.
    pub async fn evaluate_with_caller(
        &self,
        request: EvaluationRequest,
        request_id: String,
        inbound_headers: http::HeaderMap,
        caller: CallerContext,
    ) -> Result<EvaluationResult> {
        request.validate()?;
        if request_id.is_empty() {
            return Err(BitrouterError::bad_request(
                "evaluation request id must be non-empty",
            ));
        }
        let route = self.routing.resolve_evaluation_route(&request.model)?;
        validate_route_limits(&request, &route.declaration.limits)?;
        let adapter = self
            .extensions
            .evaluation_format(&route.declaration.operation.format)?;
        let cancellation = CancellationToken::new();
        let cancelled_on_drop = CancellationOnDrop(cancellation.clone());
        let (sender, receiver) = oneshot::channel();
        {
            let _admission = self
                .admission
                .lock()
                .map_err(|_| BitrouterError::internal("evaluation admission lock poisoned"))?;
            if !self.accepting.load(Ordering::Acquire) {
                return Err(BitrouterError::UpstreamUnavailable);
            }
            let http = Arc::clone(&self.http);
            let recorder = Arc::clone(&self.recorder);
            self.tasks.spawn(async move {
                let result = execute_route(
                    &http,
                    adapter.as_ref(),
                    recorder.as_ref(),
                    EvaluationJob {
                        route,
                        request,
                        request_id,
                        inbound_headers,
                        caller,
                        cancellation,
                    },
                )
                .await;
                let _ = sender.send(result);
            });
        }
        let result = receiver.await.map_err(|_| {
            BitrouterError::internal("evaluation execution ended without terminal result")
        })?;
        drop(cancelled_on_drop);
        result
    }

    /// Stop admission and wait until every detached evaluation and its
    /// terminal recorder has finished. Safe to call repeatedly.
    pub async fn drain(&self) {
        if let Ok(_admission) = self.admission.lock() {
            self.accepting.store(false, Ordering::Release);
            self.tasks.close();
        }
        self.tasks.wait().await;
    }
}

fn validate_route_limits(
    request: &EvaluationRequest,
    limits: &crate::config::ModelOperationConfig,
) -> Result<()> {
    for question in request.questions.values() {
        let (kind, count) = match question {
            EvaluationQuestion::Noul { .. } => (EvaluationQuestionType::Noul, None),
            EvaluationQuestion::Choice { criteria, .. } => {
                (EvaluationQuestionType::Choice, Some(criteria.len()))
            }
            EvaluationQuestion::Score { criteria, .. } => {
                (EvaluationQuestionType::Score, Some(criteria.len()))
            }
        };
        if !limits.question_types.contains(&kind) {
            return Err(BitrouterError::bad_request(
                "evaluation question type is not supported by this model",
            ));
        }
        if matches!(kind, EvaluationQuestionType::Choice)
            && count.is_some_and(|count| limits.max_choice_options.is_some_and(|max| count > max))
        {
            return Err(BitrouterError::bad_request(
                "evaluation choice count exceeds provider limit",
            ));
        }
        if matches!(kind, EvaluationQuestionType::Score)
            && count.is_some_and(|count| limits.max_score_levels.is_some_and(|max| count > max))
        {
            return Err(BitrouterError::bad_request(
                "evaluation score levels exceed provider limit",
            ));
        }
    }
    Ok(())
}

async fn execute_route(
    http: &HttpExecutor,
    adapter: &dyn crate::extension::evaluation_format::EvaluationFormatAdapter,
    recorder: &dyn EvaluationAttemptRecorder,
    job: EvaluationJob,
) -> Result<EvaluationResult> {
    let EvaluationJob {
        route,
        request,
        request_id,
        inbound_headers,
        caller,
        cancellation,
    } = job;
    let format = &route.declaration.operation.format;
    let format_label = format!(
        "{}/{}@{}",
        format.extension, format.adapter, format.revision
    );
    let mut saw_invalid_response = false;
    let count = route.targets.len();
    let canonical_model = route.declaration.model.clone();
    for (index, target) in route.targets.into_iter().enumerate() {
        if cancellation.is_cancelled() {
            return Err(BitrouterError::UpstreamUnavailable);
        }
        let started = Instant::now();
        let execution = http.execute_evaluation_attempt(
            &target,
            &route.declaration.operation.endpoint,
            adapter,
            &request,
            &request_id,
            &inbound_headers,
        );
        let response = tokio::select! {
            biased;
            response = execution => Some(response),
            () = cancellation.cancelled() => None,
        };
        let duration_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let base_record = EvaluationAttemptRecord {
            request_id: request_id.clone(),
            selector: request.model.clone(),
            canonical_model: canonical_model.clone(),
            caller_api_key_id: caller.api_key_id().to_owned(),
            caller_user_id: caller.user_id().to_owned(),
            provider: target.provider_name.clone(),
            provider_model_id: target.service_id.clone(),
            reported_model: None,
            account_label: target.account_label.clone(),
            attempt: index + 1,
            format: format_label.clone(),
            duration_ms,
            terminal: EvaluationAttemptTerminal::Failed,
            error_code: None,
            usage: None,
        };
        match response {
            None => {
                recorder
                    .record(EvaluationAttemptRecord {
                        terminal: EvaluationAttemptTerminal::UnknownRemoteCompletion,
                        error_code: Some("client_cancelled"),
                        ..base_record
                    })
                    .await?;
                return Err(BitrouterError::UpstreamUnavailable);
            }
            Some(Ok(mut result)) => {
                let cost = recorder
                    .record(EvaluationAttemptRecord {
                        terminal: EvaluationAttemptTerminal::Completed,
                        reported_model: Some(result.model.clone()),
                        usage: Some(result.usage.clone()),
                        ..base_record
                    })
                    .await?;
                if cost.is_some_and(|value| !value.is_finite() || value < 0.0) {
                    return Err(BitrouterError::internal(
                        "evaluation recorder returned invalid settled cost",
                    ));
                }
                result.usage.cost = cost;
                return Ok(result);
            }
            Some(Err(error)) => {
                let retry = retryable(&error) && index + 1 < count;
                let delay = match &error {
                    BitrouterError::UpstreamRateLimited {
                        retry_after: Some(seconds),
                        ..
                    } => Duration::from_secs((*seconds).min(5)),
                    _ => Duration::ZERO,
                };
                saw_invalid_response |=
                    matches!(error, BitrouterError::UpstreamInvalidResponse { .. });
                recorder
                    .record(EvaluationAttemptRecord {
                        terminal: if matches!(error, BitrouterError::UpstreamTimeout) {
                            EvaluationAttemptTerminal::TimedOut
                        } else {
                            EvaluationAttemptTerminal::Failed
                        },
                        error_code: Some(error_code(&error)),
                        ..base_record
                    })
                    .await?;
                if !retry {
                    if saw_invalid_response && retryable(&error) {
                        return Err(BitrouterError::UpstreamInvalidResponse {
                            message: "one or more evaluation accounts returned invalid responses"
                                .into(),
                        });
                    }
                    return Err(error);
                }
                if !delay.is_zero() {
                    tokio::select! {
                        () = tokio::time::sleep(delay) => {},
                        () = cancellation.cancelled() => return Err(BitrouterError::UpstreamUnavailable),
                    }
                }
            }
        }
    }
    if saw_invalid_response {
        Err(BitrouterError::UpstreamInvalidResponse {
            message: "one or more evaluation accounts returned invalid responses".into(),
        })
    } else {
        Err(BitrouterError::UpstreamUnavailable)
    }
}

fn retryable(error: &BitrouterError) -> bool {
    matches!(
        error,
        BitrouterError::UpstreamTimeout
            | BitrouterError::UpstreamRateLimited { .. }
            | BitrouterError::UpstreamInvalidResponse { .. }
            | BitrouterError::UpstreamUnavailable
            | BitrouterError::Upstream {
                status: 408 | 429 | 529 | 500..=599,
                ..
            }
    )
}

fn error_code(error: &BitrouterError) -> &'static str {
    match error {
        BitrouterError::UpstreamTimeout => "upstream_timeout",
        BitrouterError::UpstreamRateLimited { .. } => "upstream_rate_limited",
        BitrouterError::UpstreamInvalidResponse { .. } => "upstream_invalid_response",
        BitrouterError::UpstreamBadRequest { .. } => "provider_rejected_evaluation",
        BitrouterError::UpstreamUnavailable => "upstream_unavailable",
        BitrouterError::Upstream {
            status: 401 | 403, ..
        } => "upstream_authentication_failed",
        BitrouterError::Upstream { .. } => "upstream_error",
        _ => "evaluation_failed",
    }
}
