//! Existing evaluation exchange on the inference listener.

use bitrouter_ai::eval::{
    AdmissionOutcome, EvalSnapshot, EvalSubject, EvaluationResult, FreezeRequest, PutSubjectReport,
    StatusReport,
};
use reqwest::Response;
use serde::{Serialize, de::DeserializeOwned};
use url::Url;

use super::GatewayClient;
use crate::{
    error::{Error, Result},
    transport,
};

impl GatewayClient {
    /// Insert immutable evidence using this client's service credential.
    /// An interrupted submission is never automatically retried.
    pub async fn put_eval_subject(&self, subject: &EvalSubject) -> Result<PutSubjectReport> {
        self.eval_post("v1/evals/subjects", subject).await
    }

    /// List subjects visible to the authenticated principal.
    pub async fn eval_subjects(&self) -> Result<Vec<EvalSubject>> {
        self.eval_get(self.url("v1/evals/subjects")?).await
    }

    pub async fn eval_subject(&self, eval_id: &str) -> Result<EvalSubject> {
        self.eval_get(self.eval_resource("subjects", eval_id)?)
            .await
    }

    /// Submit a result for service-side admission; this never publishes a policy.
    pub async fn submit_eval_result(&self, result: &EvaluationResult) -> Result<AdmissionOutcome> {
        self.eval_post("v1/evals/results", result).await
    }

    pub async fn freeze_eval_snapshot(&self, request: &FreezeRequest) -> Result<EvalSnapshot> {
        self.eval_post("v1/evals/snapshots", request).await
    }

    pub async fn eval_snapshot(&self, evidence_root: &str) -> Result<EvalSnapshot> {
        self.eval_get(self.eval_resource("snapshots", evidence_root)?)
            .await
    }

    /// A missing core service is an HTTP error, not an empty evidence ledger.
    pub async fn eval_status(&self) -> Result<StatusReport> {
        self.eval_get(self.url("v1/evals/status")?).await
    }

    fn eval_resource(&self, collection: &str, id: &str) -> Result<Url> {
        // Url's segment builder skips dot-only segments. Reject those explicitly
        // so a caller cannot accidentally address the collection instead.
        // https://docs.rs/url/latest/url/struct.PathSegmentsMut.html#method.push
        if id.is_empty() || matches!(id, "." | "..") {
            return Err(Error::Request(
                "evaluation resource ID must be a nonempty path segment",
            ));
        }
        let mut url = self.url(&format!("v1/evals/{collection}"))?;
        url.path_segments_mut()
            .map_err(|_| Error::Options("service endpoint cannot contain path segments"))?
            .push(id);
        Ok(url)
    }

    async fn eval_get<T: DeserializeOwned>(&self, url: Url) -> Result<T> {
        let response = self.http.get(url).send().await.map_err(Error::transport)?;
        self.eval_json(response).await
    }

    async fn eval_post<T: DeserializeOwned>(
        &self,
        path: &str,
        value: &impl Serialize,
    ) -> Result<T> {
        let response = self
            .http
            .post(self.url(path)?)
            .json(value)
            .send()
            .await
            .map_err(Error::transport)?;
        self.eval_json(response).await
    }

    async fn eval_json<T: DeserializeOwned>(&self, response: Response) -> Result<T> {
        let response =
            transport::successful_as(response, self.options.max_response_bytes, Error::Evaluation)
                .await?;
        if !transport::has_content_type(&response, "application/json") {
            return Err(Error::Response("expected application/json"));
        }
        serde_json::from_slice(&transport::body(response, self.options.max_response_bytes).await?)
            .map_err(|_| Error::Response("body does not match the evaluation contract"))
    }
}
