use super::*;
use crate::gateway::shared::authority::Revision;
use std::future::Future;

struct GatedBody {
    input: Option<tokio::sync::oneshot::Receiver<Vec<u8>>>,
    polled: Arc<AtomicUsize>,
}

impl HttpBody for GatedBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        self.polled.fetch_add(1, Ordering::SeqCst);
        let Some(input) = self.input.as_mut() else {
            return Poll::Ready(None);
        };
        let result = match Pin::new(input).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        self.input = None;
        Poll::Ready(Some(
            result
                .map(|data| Frame::data(data.into()))
                .map_err(|_| std::io::Error::other("fixture body sender dropped")),
        ))
    }
}

pub(super) async fn exercise(
    fixture: &Fixture,
    upstream: &Upstream,
    store: &Store,
    serving: &Arc<Serving>,
) -> Result<()> {
    let original = serving.snapshot();
    let same = Snapshot::new(original.revision, application(fixture)?)?;
    ensure!(same.owner != original.owner);
    ensure!(serving.publish(same).is_err());
    for revision in [
        Revision {
            config_generation: 0,
            ..original.revision
        },
        Revision {
            key_generation: 0,
            ..original.revision
        },
    ] {
        ensure!(Snapshot::new(revision, application(fixture)?).is_err());
    }
    ensure!(Arc::ptr_eq(&original, &serving.snapshot()));
    drop(original);
    Box::pin(body_wait_and_key_publication(
        fixture, upstream, store, serving,
    ))
    .await?;
    Box::pin(stream_and_key_publication(fixture, store, serving)).await?;

    let original = serving.snapshot();
    for revision in [
        original.revision,
        Revision {
            config_generation: original.revision.config_generation + 1,
            key_generation: original.revision.key_generation - 1,
        },
    ] {
        ensure!(
            serving
                .publish(Snapshot::new(revision, application(fixture)?)?)
                .is_err()
        );
    }
    // Preparing a complete but unpublished config cannot bypass durable admission.
    let prior = serving.publish(Snapshot::new(
        Revision {
            config_generation: original.revision.config_generation + 1,
            ..original.revision
        },
        application(fixture)?,
    )?)?;
    ensure!(Arc::ptr_eq(&prior, &original) && prior.app.language_model().is_some());
    // Both components are valid and nonzero, but config regresses relative to
    // the newly installed candidate even though its key component advances.
    ensure!(
        serving
            .publish(Snapshot::new(
                Revision {
                    key_generation: original.revision.key_generation + 1,
                    ..original.revision
                },
                application(fixture)?,
            )?)
            .is_err()
    );
    let calls = upstream.calls.load(Ordering::SeqCst);
    let response = serving
        .router()
        .oneshot(request(
            fixture,
            "/v1/chat/completions",
            prompt("ok"),
            None,
            "unpublished",
        )?)
        .await?;
    ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
    ensure!(!response.headers().contains_key("x-bitrouter-execution-id"));
    drop(response);
    retire(serving).await?;
    ensure!(calls == upstream.calls.load(Ordering::SeqCst));
    Ok(())
}

async fn rotate(fixture: &Fixture, store: &Store, serving: &Serving) -> Result<Arc<Snapshot>> {
    let revision = serving.snapshot().revision;
    let authority = Authority::new(store.clone(), DeploymentKey::from_bytes([9; 32])?);
    let head = authority
        .rotate_correlation(revision.key_generation)
        .await?;
    ensure!(head.config_generation == revision.config_generation);
    serving.publish(Snapshot::new(head.revision(), application(fixture)?)?)
}

async fn body_wait_and_key_publication(
    fixture: &Fixture,
    upstream: &Upstream,
    store: &Store,
    serving: &Arc<Serving>,
) -> Result<()> {
    let old_owner = serving.snapshot().owner;
    let (send, input) = tokio::sync::oneshot::channel();
    let polled = Arc::new(AtomicUsize::new(0));
    let mut pending = request(
        fixture,
        "/v1/chat/completions",
        prompt("ok"),
        None,
        "old-body",
    )?;
    *pending.body_mut() = Body::new(GatedBody {
        input: Some(input),
        polled: polled.clone(),
    });
    let mut response = Box::pin(serving.router().oneshot(pending));
    ensure!(futures::poll!(response.as_mut()).is_pending());
    ensure!(polled.load(Ordering::SeqCst) > 0);
    ensure!(serving.custody.pending_for(old_owner) == 1);
    let old = rotate(fixture, store, serving).await?;
    ensure!(old.owner == old_owner && old.owner != serving.snapshot().owner);
    ensure!(!serving.retired_quiescent(&old));
    ensure!(serving.custody.pending_for(serving.snapshot().owner) == 0);
    let calls = upstream.calls.load(Ordering::SeqCst);
    send.send(serde_json::to_vec(&prompt("ok"))?)
        .map_err(|_| anyhow::anyhow!("body receiver dropped"))?;
    let response = response.await?;
    ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
    ensure!(upstream.calls.load(Ordering::SeqCst) == calls);
    drop(response);
    ensure!(
        !serving.retired_quiescent(&old),
        "unreconciled reservation lost its resource owner"
    );
    let reader = old.clone();
    retire(serving).await?;
    ensure!(serving.custody.pending_for(old_owner) == 0);
    ensure!(
        !serving.retired_quiescent(&old),
        "snapshot reader was ignored"
    );
    drop(reader);
    ensure!(serving.retired_quiescent(&old));
    ensure!(!serving.retired_quiescent(&serving.snapshot()));
    Ok(())
}

async fn stream_and_key_publication(
    fixture: &Fixture,
    store: &Store,
    serving: &Arc<Serving>,
) -> Result<()> {
    let mut body = prompt("live");
    body["stream"] = json!(true);
    let response = serving
        .router()
        .oneshot(request(
            fixture,
            "/v1/chat/completions",
            body,
            None,
            "old-stream",
        )?)
        .await?;
    ensure!(response.status() == StatusCode::OK);
    let id = response.headers()["x-bitrouter-execution-id"]
        .to_str()?
        .to_owned();
    let mut body = response.into_body();
    let frame = futures::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
        .await
        .context("missing provider data")??;
    ensure!(frame.is_data());
    drop(frame);
    let old = rotate(fixture, store, serving).await?;
    ensure!(serving.custody.pending_for(old.owner) == 1);
    ensure!(!serving.retired_quiescent(&old));
    drop(body);
    retire(serving).await?;
    ensure!(serving.retired_quiescent(&old));
    ensure!(matches!(
        fixture.engine.query(&fixture.key.hash, &id).await?.state,
        ExecutionState::Unknown { .. }
    ));
    Ok(())
}
