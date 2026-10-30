//! Qualify the public SDK against the mounted service over real HTTP transports.
use super::*;
use bitrouter_ai::eval::{FreezeRequest, SubjectInsertOutcome};
use bitrouter_sdk::{
    gateway::HttpVersion,
    tenancy::{TenancyClient, TenancyOptions},
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn public_sdk_roundtrips_owned_ledger_over_each_enabled_transport() -> Result<()> {
    tokio::time::timeout(TIMEOUT, async {
        let versions = [
            (HttpVersion::Http1Only, 1),
            #[cfg(feature = "http2")]
            (HttpVersion::Http2PriorKnowledge, 2),
        ];
        for (version, expected_wire) in versions {
            let fixture = Fixture::new().await?;
            let key = fixture.issue("sdk-owned-eval").await?;
            let seen = Arc::new(AtomicUsize::new(0));
            let capture = seen.clone();
            let router = router(&fixture)?.layer(axum::middleware::from_fn(
                move |request: axum::extract::Request, next: axum::middleware::Next| {
                    let capture = capture.clone();
                    async move {
                        let version = match request.version() {
                            ::http::Version::HTTP_11 => 1,
                            ::http::Version::HTTP_2 => 2,
                            _ => 4,
                        };
                        capture.fetch_or(version, Ordering::SeqCst);
                        next.run(request).await
                    }
                },
            ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let endpoint = format!("http://{}", listener.local_addr()?);
            let stop = tokio_util::sync::CancellationToken::new();
            let _cleanup = stop.clone().drop_guard();
            let shutdown = stop.clone();
            let server = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown.cancelled_owned())
                    .await
            }));
            let client = TenancyClient::with_options(
                &endpoint,
                key.secret.as_deref().context("secret")?,
                TenancyOptions {
                    http_version: version,
                    ..Default::default()
                },
            )?;
            let installation = &fixture.installation;
            let tenant = &fixture.tenant;
            let mut evidence = subject()?;
            evidence.eval_id = "sdk/subject α?draft=1#part".into();
            let inserted = client
                .put_eval_subject(installation, tenant, &evidence)
                .await?;
            ensure!(inserted.eval_id == evidence.eval_id);
            ensure!(inserted.outcome == SubjectInsertOutcome::Inserted);
            ensure!(
                client
                    .eval_subject(installation, tenant, &evidence.eval_id)
                    .await?
                    == evidence
            );
            ensure!(client.eval_subjects(installation, tenant).await? == vec![evidence.clone()]);
            let mut input = evaluation(&fixture, EvalVerdict::Pass, "sdk-result")?;
            input.eval_id = evidence.eval_id;
            let admitted = client
                .submit_eval_result(installation, tenant, &input)
                .await?;
            ensure!(!admitted.duplicate);
            let replay = client
                .submit_eval_result(installation, tenant, &input)
                .await?;
            ensure!(replay.duplicate && replay.result_id == admitted.result_id);
            let time = "2026-10-25T08:02:00.000+08:00";
            let frozen = client
                .freeze_eval_snapshot(
                    installation,
                    tenant,
                    &FreezeRequest {
                        frozen_at: Some(time.into()),
                    },
                )
                .await?;
            ensure!(frozen.frozen_at == time && frozen.entries.len() == 1);
            ensure!(
                client
                    .eval_snapshot(installation, tenant, &frozen.evidence_root)
                    .await?
                    == frozen
            );
            let status = client.eval_status(installation, tenant).await?;
            ensure!(status.subjects == 1 && status.results == 1);
            ensure!(status.admission.get("admitted") == Some(&1));
            ensure!(seen.load(Ordering::SeqCst) == expected_wire);
            drop(client);
            stop.cancel();
            server.await??;
            fixture.service.shutdown().await?;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await?
}
