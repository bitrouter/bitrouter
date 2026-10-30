//! Owned evaluation exchange on the protected listener, using existing DTOs.
//!
//! The service checks live tenant authority on every call. No caller/owner field
//! is synthesized and no local evaluator configuration grants permission.

mod transport;
mod validation;

use bitrouter_ai::{
    eval::{
        AdmissionOutcome, EvalSnapshot, EvalSubject, EvaluationResult, FreezeRequest,
        PutSubjectReport, StatusReport,
    },
    tenancy::identity::{InstallationId, TenantId},
};
use reqwest::{Method, RequestBuilder, header};
use serde::Serialize;
use url::Url;

use super::TenancyClient;
use crate::error::{Error, Result};
use validation::EvalReply;

const EVIDENCE_BYTES: usize = 1024 * 1024;
const FREEZE_BYTES: usize = 4 * 1024;

impl TenancyClient {
    /// Insert immutable evidence in the explicitly addressed tenant's ledger.
    /// Requires current `evaluation.submit` authority; never retries.
    ///
    /// # Errors
    /// Local encoding failures precede dispatch. Any later failure is
    /// [`Error::OwnedEvalUnknown`], including a post-commit authorization denial.
    pub async fn put_eval_subject(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        subject: &EvalSubject,
    ) -> Result<PutSubjectReport> {
        self.owned_eval_post(
            installation,
            self.owned_eval_url(tenant, "subjects", None)?,
            subject,
            EVIDENCE_BYTES,
            |report: &PutSubjectReport| matching(report.eval_id == subject.eval_id),
        )
        .await
    }

    /// List the addressed tenant's subjects under current `resource.read` authority.
    /// The service bounds the collection; this call does not paginate or cache.
    ///
    /// # Errors
    /// Returns typed tenancy failures, transport failures or bounded response errors.
    pub async fn eval_subjects(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
    ) -> Result<Vec<EvalSubject>> {
        self.owned_eval_get(
            installation,
            self.owned_eval_url(tenant, "subjects", None)?,
            |_| Ok(()),
        )
        .await
    }

    /// Read one subject from the explicit tenant namespace with `resource.read`.
    /// The logical ID is encoded as one URL segment, never interpreted as a path.
    ///
    /// # Errors
    /// Also rejects invalid path IDs and a response naming a different subject.
    pub async fn eval_subject(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        eval_id: &str,
    ) -> Result<EvalSubject> {
        self.owned_eval_get(
            installation,
            self.owned_eval_url(tenant, "subjects", Some(eval_id))?,
            |subject: &EvalSubject| matching(subject.eval_id == eval_id),
        )
        .await
    }

    /// Submit evidence under current `evaluation.submit` and evaluator authority.
    /// Inspect the receipt's status: a successful HTTP response need not mean
    /// admission. The existing result and idempotency fields are sent unchanged.
    /// The receipt does not echo them; its opaque result ID cannot independently
    /// bind the reply to the submitted result. The endpoint must be trusted.
    ///
    /// # Errors
    /// Local encoding failures precede dispatch; subsequent failures are
    /// [`Error::OwnedEvalUnknown`]. No retry or new identity is synthesized.
    pub async fn submit_eval_result(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        result: &EvaluationResult,
    ) -> Result<AdmissionOutcome> {
        self.owned_eval_post(
            installation,
            self.owned_eval_url(tenant, "results", None)?,
            result,
            EVIDENCE_BYTES,
            |_| Ok(()),
        )
        .await
    }

    /// Freeze evidence with both `resource.read` and `evaluation.submit`.
    /// Sends the existing JSON DTO; `FreezeRequest::default()` uses service time.
    /// An explicit timestamp must be returned with the same string spelling.
    /// Freeze has no client identity guaranteeing exactly-once recovery.
    ///
    /// # Errors
    /// Local encoding failures precede dispatch; subsequent failures are
    /// [`Error::OwnedEvalUnknown`], with no automatic resubmission.
    pub async fn freeze_eval_snapshot(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        request: &FreezeRequest,
    ) -> Result<EvalSnapshot> {
        self.owned_eval_post(
            installation,
            self.owned_eval_url(tenant, "snapshots", None)?,
            request,
            FREEZE_BYTES,
            |snapshot: &EvalSnapshot| {
                matching(
                    request
                        .frozen_at
                        .as_ref()
                        .is_none_or(|expected| expected == &snapshot.frozen_at),
                )
            },
        )
        .await
    }

    /// Read the named immutable snapshot with current `resource.read` authority.
    ///
    /// # Errors
    /// Rejects invalid path IDs, failed responses and a different returned root.
    pub async fn eval_snapshot(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
        evidence_root: &str,
    ) -> Result<EvalSnapshot> {
        self.owned_eval_get(
            installation,
            self.owned_eval_url(tenant, "snapshots", Some(evidence_root))?,
            |snapshot: &EvalSnapshot| matching(snapshot.evidence_root == evidence_root),
        )
        .await
    }

    /// Read fresh counts in the addressed tenant ledger with `resource.read`.
    ///
    /// # Errors
    /// An unavailable endpoint is an error, never an empty ledger.
    pub async fn eval_status(
        &self,
        installation: &InstallationId,
        tenant: &TenantId,
    ) -> Result<StatusReport> {
        self.owned_eval_get(
            installation,
            self.owned_eval_url(tenant, "status", None)?,
            |_| Ok(()),
        )
        .await
    }

    fn owned_eval_url(&self, tenant: &TenantId, collection: &str, id: Option<&str>) -> Result<Url> {
        let mut url = self.url(&format!("tenants/{tenant}/evals/{collection}"))?;
        if let Some(id) = id {
            // Dot-only segments are skipped by Url::path_segments_mut. Reject
            // them rather than silently requesting the collection.
            if id.trim().is_empty()
                || id.len() > 512
                || id.chars().any(char::is_control)
                || matches!(id, "." | "..")
            {
                return Err(Error::Request("invalid evaluation resource path ID"));
            }
            url.path_segments_mut()
                .map_err(|_| Error::Options("invalid evaluation endpoint"))?
                .push(id);
        }
        Ok(url)
    }

    fn owned_eval_request(
        &self,
        method: Method,
        url: Url,
        installation: &InstallationId,
    ) -> RequestBuilder {
        self.http
            .request(method, url)
            .header("Bitrouter-Installation-Id", installation.as_str())
            .header(header::CACHE_CONTROL, "no-store")
    }

    async fn owned_eval_get<T: EvalReply>(
        &self,
        installation: &InstallationId,
        url: Url,
        check: impl FnOnce(&T) -> Result<()>,
    ) -> Result<T> {
        self.owned_eval_send(
            self.owned_eval_request(Method::GET, url, installation),
            check,
        )
        .await
    }

    async fn owned_eval_post<I: Serialize, O: EvalReply>(
        &self,
        installation: &InstallationId,
        url: Url,
        input: &I,
        limit: usize,
        check: impl FnOnce(&O) -> Result<()>,
    ) -> Result<O> {
        let body = transport::encode(input, limit)?;
        let request = self
            .owned_eval_request(Method::POST, url, installation)
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
        self.owned_eval_send(request, check)
            .await
            .map_err(|source| Error::OwnedEvalUnknown {
                source: Box::new(source),
            })
    }
}

fn matching(matches: bool) -> Result<()> {
    if matches {
        Ok(())
    } else {
        Err(Error::Response(
            "evaluation response identity differs from request",
        ))
    }
}
