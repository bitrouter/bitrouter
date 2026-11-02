#[path = "transport_protocol/server.rs"]
mod server;
use axum::{
    body::{Body, Bytes},
    http::Version,
    response::Response,
};
use bitrouter_ai::tenancy::{
    identity::{ExecutionId, InstallationId, TenantId},
    usage_evidence::{Capture, MAX_BYTES, UsageEvidenceCapabilitiesV1},
};
use bitrouter_sdk::{
    error::Error,
    tenancy::{TenancyClient, TenancyOptions},
};
use serde_json::json;
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const TOKEN: &str = "fixture-tenancy-token-0123456789abcdef";

fn padded_evidence(size: usize, chunked: bool) -> TestResult<Response> {
    let mut bytes = serde_json::to_vec(
        &json!({"schema_version":1,"installation_id":"installation","tenant_id":"tenant","execution_id":"execution","capture":"not_captured","extractor_version":1,"estimation":"not_captured","estimate":null,"parent_execution_id":null,"child_execution_ids":[],"attempts":[],"frames":[],"truncated":false,"dropped_frames":0}),
    )?;
    assert!(bytes.len() < size);
    bytes.resize(size, b' ');
    let body = if chunked {
        let chunks = bytes
            .chunks(1024)
            .map(Bytes::copy_from_slice)
            .collect::<Vec<_>>();
        Body::from_stream(futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, std::io::Error>),
        ))
    } else {
        Body::from(bytes)
    };
    Ok(Response::builder()
        .header("content-type", "application/json")
        .body(body)?)
}

#[tokio::test]
async fn original_body_limit_includes_whitespace_for_sized_and_chunked_responses() -> TestResult {
    let installation: InstallationId = "installation".try_into()?;
    let tenant: TenantId = "tenant".try_into()?;
    let execution: ExecutionId = "execution".try_into()?;
    for chunked in [false, true] {
        let replies = vec![
            Some(padded_evidence(MAX_BYTES, chunked)?),
            Some(padded_evidence(MAX_BYTES + 1, chunked)?),
            Some(padded_evidence(1024, chunked)?),
            Some(padded_evidence(1025, chunked)?),
        ];
        let server = server::serve(Version::HTTP_11, replies).await?;
        for limit in [2 * 1024 * 1024, 1024] {
            let client = TenancyClient::with_options(
                &server.endpoint,
                TOKEN,
                TenancyOptions {
                    max_response_bytes: limit,
                    ..TenancyOptions::default()
                },
            )?;
            assert_eq!(
                client
                    .usage_evidence(&installation, &tenant, &execution)
                    .await?
                    .capture,
                Capture::NotCaptured
            );
            let error = client
                .usage_evidence(&installation, &tenant, &execution)
                .await;
            let Err(Error::Tenancy(failure)) = error else {
                return Err("oversized raw body was not rejected before decoding".into());
            };
            assert_eq!(failure.status, 200);
            assert!(failure.body.is_none() && matches!(failure.body_error, Some(Error::Limit)));
        }
        assert_eq!(server.records.lock().await.len(), 4);
    }
    Ok(())
}

#[tokio::test]
async fn separate_routes_validate_all_bindings_and_reject_unknown_fields() -> TestResult {
    let installation: InstallationId = "installation".try_into()?;
    let tenant: TenantId = "tenant".try_into()?;
    let execution: ExecutionId = "execution".try_into()?;
    let evidence = json!({"schema_version":1,"installation_id":"installation","tenant_id":"tenant","execution_id":"execution","capture":"not_captured","extractor_version":1,"estimation":"not_captured","estimate":null,"parent_execution_id":null,"child_execution_ids":[],"attempts":[],"frames":[],"truncated":false,"dropped_frames":0});
    let mut wrong = evidence.clone();
    wrong["tenant_id"] = json!("foreign");
    let mut extra = evidence.clone();
    extra["raw"] = json!({});
    let replies = vec![
        serde_json::to_value(UsageEvidenceCapabilitiesV1::new(installation.clone()))?,
        evidence,
        wrong,
        extra,
    ]
    .into_iter()
    .map(|v| server::json(v).map(Some))
    .collect::<TestResult<Vec<_>>>()?;
    let server = server::serve(Version::HTTP_11, replies).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    assert_eq!(
        client
            .usage_evidence_capabilities(&installation)
            .await?
            .schema_version,
        1
    );
    assert_eq!(
        client
            .usage_evidence(&installation, &tenant, &execution)
            .await?
            .capture,
        Capture::NotCaptured
    );
    assert!(matches!(
        client
            .usage_evidence(&installation, &tenant, &execution)
            .await,
        Err(Error::Response(_))
    ));
    assert!(
        client
            .usage_evidence(&installation, &tenant, &execution)
            .await
            .is_err()
    );
    let records = server.records.lock().await;
    assert_eq!(records.len(), 4);
    assert_eq!(
        records[0].path,
        "/control/v1/tenancy/usage-evidence/capabilities"
    );
    for (index, record) in records.iter().enumerate() {
        if index > 0 {
            assert_eq!(
                record.path,
                "/control/v1/tenants/tenant/executions/execution/usage-evidence"
            );
        }
        assert_eq!(record.method, "GET");
        assert_eq!(record.version, Version::HTTP_11);
        assert!(record.connection > 0 && record.body.is_empty());
        assert_eq!(
            record
                .headers
                .get("bitrouter-installation-id")
                .ok_or("installation header")?,
            "installation"
        );
    }
    Ok(())
}
