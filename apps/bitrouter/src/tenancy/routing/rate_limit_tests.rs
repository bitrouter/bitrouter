//! Local RPM gates must not masquerade as observed provider responses.

use super::super::{Fixture, failure};
use super::*;
use crate::tenancy::{Service, receipts::read, resources::Resources};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
    execution::ExecutionReceipt,
};

async fn provision_chain(
    base: &str,
    provider_limit: bool,
    local: bool,
) -> Result<(Fixture, ResourceRef)> {
    let mut fixture = Fixture::new().await?;
    fixture.service.shutdown().await?;
    let resources = Resources::with_key(fixture.installation.clone(), rand::random())?;
    let reference = |kind, id: &str| -> Result<ResourceRef> {
        Ok(ResourceRef {
            owner_kind: OwnerKind::Tenant,
            owner_id: fixture.tenant.as_str().try_into()?,
            resource_kind: kind,
            resource_id: id.try_into()?,
            revision_id: "r1".try_into()?,
        })
    };
    let route = reference(ResourceKind::Route, MODEL)?;
    let credential = reference(ResourceKind::Credential, "rate-slot")?;
    let mut first = json!({"api_base":base,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"}]});
    if local {
        if provider_limit {
            first["rate_limits"] = json!([{"*":{"requests_per_minute":0}}]);
        } else {
            first["models"][0]["rate_limits"] = json!({"requests_per_minute":0});
        }
    }
    let second = json!({"api_base":base,"api_protocol":[{"*":"chat_completions"}],"models":[{"id":"model-a"}]});
    resources.provision_bytes(&fixture.db, &serde_json::to_vec(&json!({
        "format":1,"installation_id":fixture.installation,"resources":[
            {"reference":credential,"definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"owned-upstream-secret"}}}},
            {"reference":route,"definition":{"kind":"route","selector":"rate-chain","policy":null,
                "routing":{"providers":{"first":first,"second":second},"models":{"rate-chain":{"strategy":"priority","endpoints":[
                    {"provider":"first","service_id":"model-a"},{"provider":"second","service_id":"model-a"}
                ]}}},
                "credentials":[{"provider":"first","account":"default","credential":credential},{"provider":"second","account":"default","credential":credential}]}}
        ]
    }))?).await?;
    let snapshot = crate::tenancy::store::load(&fixture.db, chrono::Utc::now()).await?;
    fixture.service =
        Service::start_prepared(fixture.db.clone(), None, snapshot, Arc::new(resources)).await?;
    Ok((fixture, route))
}

async fn rate_limit_campaign(local: bool, provider_limit: bool, stream: bool) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}/v1", listener.local_addr()?);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let provider = Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move || {
            let observed = observed.clone();
            async move {
                observed.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "12")],
                    axum::Json(json!({"error":{"message":"provider quota detail"}})),
                )
            }
        }),
    );
    let _provider = AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(listener, provider).await
    }));
    let (fixture, route) = provision_chain(&base, provider_limit, local).await?;
    let revision = draft(&fixture, &route, "rate-config", 25).await?;
    let credential = fixture.issue("rate-client").await?;
    fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let token = credential.secret.as_deref().context("caller token")?;
    let request_headers = headers(&fixture, token, &revision.revision_id)?;
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let ingress = format!("http://{}/v1/chat/completions", listener.local_addr()?);
    let _gateway =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(TIMEOUT)
        .build()?;
    let checked: Result<()> = async {
        let response = client.post(ingress).headers(request_headers)
            .json(&json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":stream})).send().await?;
        ensure!(response.status() == StatusCode::TOO_MANY_REQUESTS);
        ensure!(response.headers()["retry-after"] == if local { "61" } else { "12" });
        ensure!(response.headers().get("x-bitrouter-error-source").and_then(|value| value.to_str().ok()) == if local { None } else { Some("upstream") });
        let id = response.headers().get("x-bitrouter-request-id").context("execution ID")?.to_str()?.try_into()?;
        let body: serde_json::Value = response.json().await?;
        ensure!(body["error"]["type"] == "rate_limit_error");
        ensure!(body["error"]["code"] == if local { "rate_limit_exceeded" } else { "upstream_rate_limited" });
        ensure!(!body.to_string().contains("provider quota detail"));
        tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
        let expected_sends = if local { 0 } else { 2 };
        ensure!(calls.load(Ordering::SeqCst) == expected_sends, "local refusal must stop; explicit upstream 429 may fall back");
        let caller = fixture.service.authenticate(token).await.map_err(failure)?;
        let bytes = fixture.service.read_receipt(caller, fixture.tenant.clone(), read::Request::One(id)).await.map_err(failure)?;
        let receipt: ExecutionReceipt = serde_json::from_slice(&bytes)?;
        ensure!(receipt.attempts.len() == expected_sends, "only actual sends may be receipt attempts");
        Ok(())
    }.await;
    let stopped = tokio::time::timeout(TIMEOUT, fixture.service.shutdown()).await;
    checked?;
    stopped??;
    Ok(())
}

#[tokio::test]
async fn local_rpm_refusal_is_local_429_without_provider_sends_or_fallback() -> Result<()> {
    for provider_limit in [false, true] {
        for stream in [false, true] {
            rate_limit_campaign(true, provider_limit, stream).await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn upstream_429_retains_source_delay_and_explicit_fallback() -> Result<()> {
    for stream in [false, true] {
        rate_limit_campaign(false, false, stream).await?;
    }
    Ok(())
}
