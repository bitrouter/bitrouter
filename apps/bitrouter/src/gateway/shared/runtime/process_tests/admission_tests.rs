//! In-process router regressions: no database, stock binary, or socket required.

use tower::ServiceExt as _;

use super::*;

#[tokio::test]
async fn rejected_http_requests_are_counted_before_extraction() -> Result<()> {
    let (state, mut arrivals) = MockState::new();
    let app = mock_router(state.clone());
    let valid = serde_json::to_string(&json!({"messages":[{"content":"tag"}]}))?;
    let cases = [
        (
            Method::POST,
            "/tenant/chat/completions",
            Some("application/json"),
            "{",
            StatusCode::BAD_REQUEST,
            INVALID_JSON,
        ),
        (
            Method::POST,
            "/tenant/chat/completions",
            Some("text/plain"),
            valid.as_str(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            INVALID_JSON,
        ),
        (
            Method::POST,
            "/tenant/chat/completions",
            None,
            valid.as_str(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            INVALID_JSON,
        ),
        (
            Method::GET,
            "/tenant/chat/completions",
            Some("application/json"),
            valid.as_str(),
            StatusCode::METHOD_NOT_ALLOWED,
            INVALID_METHOD,
        ),
        (
            Method::POST,
            "/wrong/chat/completions",
            Some("application/json"),
            valid.as_str(),
            StatusCode::NOT_FOUND,
            INVALID_PATH,
        ),
        (
            Method::POST,
            "/tenant/chat/completions?extra=1",
            Some("application/json"),
            valid.as_str(),
            StatusCode::NOT_FOUND,
            INVALID_PATH,
        ),
        (
            Method::POST,
            "/tenant/chat/completions",
            Some("application/json"),
            "{}",
            StatusCode::BAD_REQUEST,
            INVALID_TAG,
        ),
    ];
    let mut kinds = 0;
    for (index, (method, path, content_type, body, status, kind)) in cases.into_iter().enumerate() {
        let mut request = Request::builder().method(method).uri(path);
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        let response = app
            .clone()
            .oneshot(request.body(Body::from(body.to_owned()))?)
            .await?;
        ensure!(response.status() == status);
        ensure!(state.calls.load(Ordering::SeqCst) == index + 1);
        ensure!(state.rejected.load(Ordering::SeqCst) == index + 1);
        kinds |= kind;
        ensure!(state.rejection_kinds.load(Ordering::SeqCst) == kinds);
        ensure!(
            matches!(arrivals.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
            "rejected request became a releasable provider arrival"
        );
        ensure!(
            state.validate().is_err(),
            "mock validation forgot a rejected HTTP request"
        );
    }
    Ok(())
}

#[tokio::test]
async fn valid_mock_arrival_keeps_barrier_and_extra_invalid_dispatch_is_counted() -> Result<()> {
    let (state, mut arrivals) = MockState::new();
    let app = mock_router(state.clone());
    ensure!(state.validate().is_ok());
    let execution = uuid::Uuid::new_v4().to_string();
    let request = Request::builder()
        .method(Method::POST)
        .uri("/account/chat/completions")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {}", SECRETS[1]))
        .header("x-bitrouter-request-id", &execution)
        .body(Body::from(serde_json::to_vec(
            &json!({"messages":[{"content":"held"}]}),
        )?))?;
    let exercise = async {
        let response = app.clone().oneshot(request);
        tokio::pin!(response);
        let arrival = tokio::select! {
            response = &mut response => {
                anyhow::bail!("provider replied before explicit release: {}", response?.status());
            }
            arrival = arrivals.recv() => arrival.context("missing valid provider arrival")?,
        };
        ensure!(state.calls.load(Ordering::SeqCst) == 1);
        ensure!(arrival.tag == "held" && arrival.execution == execution);
        ensure!(arrival.path == "/account/chat/completions");
        ensure!(arrival.authorization == format!("Bearer {}", SECRETS[1]));
        state.validate()?;
        arrival.release(Reply::Success)?;
        ensure!(response.await?.status() == StatusCode::OK);
        // An extra malformed dispatch is counted separately from valid arrivals.
        let rejected = app
            .clone()
            .oneshot(
                Request::post("/account/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from("{"))?,
            )
            .await?;
        ensure!(rejected.status() == StatusCode::BAD_REQUEST);
        ensure!(state.calls.load(Ordering::SeqCst) == 2);
        ensure!(state.rejected.load(Ordering::SeqCst) == 1);
        ensure!(state.validate().is_err());
        Ok::<(), anyhow::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(1), exercise)
        .await
        .context("mock barrier test timed out")??;
    Ok(())
}

#[tokio::test]
async fn mock_cleanup_aggregates_root_failure_and_request_validation() -> Result<()> {
    let (state, arrivals) = MockState::new();
    let rejected = mock_router(state.clone())
        .oneshot(
            Request::post("/tenant/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from("{"))?,
        )
        .await?;
    ensure!(rejected.status() == StatusCode::BAD_REQUEST);
    let mut roots = JoinSet::new();
    roots.spawn(async { Err(std::io::Error::other("MOCK_ROOT_FAILURE")) });
    let mock = Mock {
        base: String::new(),
        arrivals,
        state,
        roots,
    };
    let error = mock
        .finish()
        .await
        .err()
        .context("mock cleanup hid both failures")?;
    let diagnostic = format!("{error:#}");
    ensure!(diagnostic.contains("MOCK_ROOT_FAILURE"));
    ensure!(diagnostic.contains("mock rejected 1 HTTP requests"));
    Ok(())
}
