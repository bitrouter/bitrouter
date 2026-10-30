use super::*;

#[derive(Clone)]
struct Wire {
    calls: Arc<AtomicUsize>,
    received: mpsc::Sender<Observed>,
    release: CancellationToken,
    stop: CancellationToken,
    invalid: bool,
    hide_optional: bool,
    broken_body: bool,
    model: Arc<String>,
    pause_at: Arc<AtomicUsize>,
    resume: CancellationToken,
}

async fn serve(
    State(wire): State<Wire>,
    version: ::http::Version,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let ordinal = wire.calls.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(questions) = body["questions"].as_array() else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let mut answers = Vec::new();
    for question in questions {
        let Some(choices) = question["choices"].as_array() else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let selected = if choices.iter().any(|value| value["value"] == "unknown") {
            "unknown"
        } else if wire.hide_optional && choices.iter().any(|value| value["value"] == "hide") {
            "hide"
        } else {
            "full"
        };
        if !choices.iter().any(|value| value["value"] == selected) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        answers.push(json!({"type":"choice","name":question["name"],"choice":selected,
            "confidence": if wire.invalid { 2.0 } else { 1.0 },
            "probabilities":choices.iter().map(|value| json!({"value":value["value"],
                "probability":if value["value"] == selected {1.0} else {0.0}})).collect::<Vec<_>>() }));
    }
    if wire
        .received
        .try_send(Observed {
            version,
            path: "/v1/decisions".into(),
            headers,
            body,
        })
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    tokio::select! {
        () = wire.stop.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        () = wire.release.cancelled() => {},
    }
    if ordinal >= wire.pause_at.load(Ordering::SeqCst) {
        tokio::select! {
            () = wire.stop.cancelled() => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            () = wire.resume.cancelled() => {},
        }
    }
    if wire.broken_body {
        // A real socket receives the request and fails while returning bytes.
        // This is after application send admission, not a fabricated receipt.
        return Response::new(axum::body::Body::from_stream(futures::stream::once(
            async {
                Err::<axum::body::Bytes, _>(std::io::Error::other("fixture transport body failed"))
            },
        )));
    }
    // Native Decisions follows the project's first-party typed Decisions contract;
    // it is deliberately a distinct response from the Legacy SystemOne schema.
    (
        StatusCode::OK,
        Json(json!({"model":wire.model.as_str(),"answers":answers,
        "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10,
            "input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0},
            "output_tokens_details":{"reasoning_tokens":0}}})),
    )
        .into_response()
}

pub(in crate::tenancy::tests::execution::native) struct Endpoint {
    pub url: String,
    wire: Wire,
    received: mpsc::Receiver<Observed>,
    task: Option<AbortOnDropHandle<std::io::Result<()>>>,
}
impl Endpoint {
    pub async fn start(held: bool, invalid: bool) -> Result<Self> {
        Self::build(held, invalid, false, false, "judge-reported-r1".into()).await
    }
    pub async fn hiding() -> Result<Self> {
        Self::build(false, false, true, false, "judge-reported-r1".into()).await
    }
    #[cfg(bitrouter_capacity_tests)]
    pub async fn large() -> Result<Self> {
        // A provider may report a concrete model name. Keep the real result below
        // the frozen 270336-byte allowance while materially filling receipt history.
        Self::build(
            false,
            false,
            false,
            false,
            format!("judge-{}", "x".repeat(240 * 1024)),
        )
        .await
    }
    pub async fn transport_failure() -> Result<Self> {
        Self::build(false, false, false, true, "judge-reported-r1".into()).await
    }
    #[cfg(bitrouter_capacity_tests)]
    pub fn pause_next(&self) {
        self.wire.pause_at.store(self.calls() + 1, Ordering::SeqCst);
    }
    #[cfg(bitrouter_capacity_tests)]
    pub fn resume(&self) {
        self.wire.resume.cancel();
    }
    async fn build(
        held: bool,
        invalid: bool,
        hide_optional: bool,
        broken_body: bool,
        model: String,
    ) -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/v1", listener.local_addr()?);
        let (sent, received) = mpsc::channel(16);
        let wire = Wire {
            calls: Arc::new(AtomicUsize::new(0)),
            received: sent,
            release: CancellationToken::new(),
            stop: CancellationToken::new(),
            invalid,
            hide_optional,
            broken_body,
            model: Arc::new(model),
            pause_at: Arc::new(AtomicUsize::new(usize::MAX)),
            resume: CancellationToken::new(),
        };
        if !held {
            wire.release.cancel();
        }
        let router = Router::new()
            .route("/v1/decisions", post(serve))
            .with_state(wire.clone());
        let stop = wire.stop.clone();
        let task = AbortOnDropHandle::new(tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
        }));
        Ok(Self {
            url,
            wire,
            received,
            task: Some(task),
        })
    }
    pub fn calls(&self) -> usize {
        self.wire.calls.load(Ordering::SeqCst)
    }
    pub fn release(&self) {
        self.wire.release.cancel();
    }
    pub async fn receive(&mut self) -> Result<Observed> {
        self.receive_until(tokio::time::Instant::now() + TIMEOUT)
            .await
    }
    pub async fn receive_until(&mut self, deadline: tokio::time::Instant) -> Result<Observed> {
        tokio::time::timeout_at(deadline, self.received.recv())
            .await?
            .context("native Decisions request")
    }
    pub async fn finish(&mut self) -> Result<()> {
        self.wire.release.cancel();
        self.wire.resume.cancel();
        self.wire.stop.cancel();
        if let Some(task) = self.task.take() {
            tokio::time::timeout(TIMEOUT, task).await???;
        }
        Ok(())
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.wire.release.cancel();
        self.wire.resume.cancel();
        self.wire.stop.cancel();
    }
}
