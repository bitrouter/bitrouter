//! Actual sockets exercise the closed read contract without mutation recovery.
#[path = "transport_protocol/server.rs"]
mod server;
use axum::{body::Body, http::Version, response::Response};
use bitrouter_ai::tenancy::profile_observation::AdmissionProfileObservationRequestV1;
use bitrouter_sdk::{
    error::Error,
    gateway::HttpVersion,
    tenancy::{TenancyClient, TenancyOptions},
};
use serde_json::{Value, json};
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn versions() -> Vec<(HttpVersion, Version)> {
    vec![
        (HttpVersion::Http1Only, Version::HTTP_11),
        #[cfg(feature = "http2")]
        (HttpVersion::Http2PriorKnowledge, Version::HTTP_2),
    ]
}
fn reference(pin: &str) -> Value {
    json!({"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy","resource_id":"member","revision_id":pin})
}
fn input() -> TestResult<AdmissionProfileObservationRequestV1> {
    Ok(serde_json::from_value(
        json!({"schema_version":1,"expected_authority_revision":7,"profiles":[reference("pin"),reference("older")]}),
    )?)
}
fn snapshot() -> Value {
    let profiles=["pin","older"].map(|pin|json!({"profile":reference(pin),"current_profile_revision":"head","rate":{"member_rpm":10,"shared_count":2},"observed_shared_count":2,"health":"healthy","availability":"eligible"}));
    json!({"schema_version":1,"installation_id":"installation","instance_id":"instance","authority_revision":7,"observed_at":"2026-11-03T00:00:00Z","profiles":profiles})
}

fn capabilities() -> Value {
    json!({"contract_version":1,"installation_id":"installation","instance_id":"instance","authority_revision":7,"observation_enabled":true,"max_profiles":256,"max_request_bytes":1048576,"max_response_bytes":1048576,"profile_versions":[1,2]})
}
fn client(server: &server::Server, version: HttpVersion) -> TestResult<TenancyClient> {
    Ok(TenancyClient::with_options(
        &server.endpoint,
        "operator-token",
        TenancyOptions {
            http_version: version,
            ..Default::default()
        },
    )?)
}

#[tokio::test]
async fn discovery_and_ordered_profile_reads_use_exact_h1_h2_paths_and_headers() -> TestResult {
    for (transport, wire) in versions() {
        let mut replies = vec![Some(server::json(capabilities())?)];
        for rate in [true, false] {
            for observed in [true, false] {
                let mut value = snapshot();
                for entry in value["profiles"].as_array_mut().ok_or("profiles")? {
                    if !rate {
                        entry["rate"] = Value::Null;
                    }
                    if !observed {
                        entry["observed_shared_count"] = Value::Null;
                    }
                }
                replies.push(Some(server::json(value)?));
            }
        }
        let server = server::serve(wire, replies).await?;
        let client = client(&server, transport)?;
        let caps = client.admission_profile_observation_capabilities().await?;
        assert!(caps.observation_enabled);
        assert_eq!(caps.profile_versions, [1, 2]);
        let input = input()?;
        for rate in [true, false] {
            for observed in [true, false] {
                let value = client
                    .observe_admission_profiles(&caps.installation_id, &input)
                    .await?;
                assert_eq!(value.profiles.len(), 2);
                for (entry, pin) in value.profiles.iter().zip(&input.profiles) {
                    assert_eq!(&entry.profile, pin);
                    assert_eq!(entry.rate.is_some(), rate);
                    assert_eq!(entry.observed_shared_count.is_some(), observed);
                }
            }
        }
        let records = server.records.lock().await;
        assert_eq!(records.len(), 5);
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record.version, wire);
            assert_eq!(record.connection, records[0].connection);
            assert_eq!(record.headers["authorization"], "Bearer operator-token");
            assert!(!record.headers.contains_key("bitrouter-operation-id"));
            if index == 0 {
                assert_eq!(record.method, "GET");
                assert_eq!(
                    record.path,
                    "/control/v1/tenancy/admission-profiles/observations/capabilities"
                );
                assert!(record.body.is_empty());
                assert!(!record.headers.contains_key("bitrouter-installation-id"));
            } else {
                assert_eq!(record.method, "POST");
                assert_eq!(
                    record.path,
                    "/control/v1/tenancy/admission-profiles/observations/v1"
                );
                assert_eq!(record.headers["bitrouter-installation-id"], "installation");
                assert_eq!(
                    serde_json::from_slice::<Value>(&record.body)?,
                    serde_json::to_value(&input)?
                );
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn mismatched_authority_identity_order_and_counts_fail_without_retry() -> TestResult {
    for (transport, wire) in versions() {
        let mut cases = Vec::new();
        for (path, value) in [
            ("/installation_id", json!("other")),
            ("/authority_revision", json!(8)),
            ("/profiles/0/profile/revision_id", json!("different")),
            ("/profiles/0/observed_shared_count", json!(9)),
            ("/profiles/0/rate/shared_count", json!(9007199254740992_u64)),
            ("/profiles/0/current_profile_revision", Value::Null),
        ] {
            let mut changed = snapshot();
            *changed.pointer_mut(path).ok_or("field")? = value;
            cases.push(changed);
        }
        let mut swapped = snapshot();
        swapped["profiles"]
            .as_array_mut()
            .ok_or("profiles")?
            .swap(0, 1);
        cases.push(swapped);
        let mut missing = snapshot();
        missing["profiles"].as_array_mut().ok_or("profiles")?.pop();
        cases.push(missing);
        let count = cases.len();
        let replies = cases
            .into_iter()
            .map(server::json)
            .collect::<TestResult<Vec<_>>>()?
            .into_iter()
            .map(Some)
            .collect();
        let server = server::serve(wire, replies).await?;
        let client = client(&server, transport)?;
        for _ in 0..count {
            assert!(
                client
                    .observe_admission_profiles(&"installation".try_into()?, &input()?)
                    .await
                    .is_err()
            );
        }
        assert_eq!(server.records.lock().await.len(), count);
    }
    Ok(())
}

#[tokio::test]
async fn duplicate_unknown_missing_nullable_and_fractional_wire_fields_fail() -> TestResult {
    let raw = serde_json::to_string(&snapshot())?;
    let cases = vec![
        raw.replace(
            "\"schema_version\":1",
            "\"schema_version\":1,\"schema_version\":1",
        ),
        raw.replacen('{', "{\"unknown\":null,", 1),
        raw.replace("\"observed_shared_count\":2,", ""),
        raw.replace(
            "\"shared_count\":2",
            "\"shared_count\":2.000000000000000001",
        ),
        raw.replace("\"schema_version\":1", "\"schema_version\":1.1"),
    ];
    let count = cases.len();
    let replies = cases
        .into_iter()
        .map(|body| {
            Ok(Some(
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(body))?,
            ))
        })
        .collect::<TestResult<Vec<_>>>()?;
    let server = server::serve(Version::HTTP_11, replies).await?;
    let client = client(&server, HttpVersion::Http1Only)?;
    for _ in 0..count {
        assert!(
            client
                .observe_admission_profiles(&"installation".try_into()?, &input()?)
                .await
                .is_err()
        );
    }
    assert_eq!(server.records.lock().await.len(), count);
    Ok(())
}

#[tokio::test]
async fn invalid_queries_are_local_and_lost_reads_are_never_unknown_operations() -> TestResult {
    let server = server::serve(Version::HTTP_11, vec![None]).await?;
    let client = client(&server, HttpVersion::Http1Only)?;
    let mut invalid = input()?;
    invalid.profiles.clear();
    assert!(matches!(
        client
            .observe_admission_profiles(&"installation".try_into()?, &invalid)
            .await,
        Err(Error::Request(_))
    ));
    assert!(server.records.lock().await.is_empty());
    let error = client
        .observe_admission_profiles(&"installation".try_into()?, &input()?)
        .await
        .err()
        .ok_or("expected transport loss")?;
    assert!(!matches!(error, Error::TenancyUnknown { .. }));
    assert_eq!(server.records.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn original_response_bytes_and_client_bounds_apply_before_decoding() -> TestResult {
    let mut padded = serde_json::to_vec(&snapshot())?;
    padded.resize(1048577, b' ');
    let server = server::serve(
        Version::HTTP_11,
        vec![
            Some(
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(padded))?,
            ),
            Some(server::json(snapshot())?),
        ],
    )
    .await?;
    let normal = client(&server, HttpVersion::Http1Only)?;
    assert!(
        normal
            .observe_admission_profiles(&"installation".try_into()?, &input()?)
            .await
            .is_err()
    );
    let bounded = TenancyClient::with_options(
        &server.endpoint,
        "operator-token",
        TenancyOptions {
            max_response_bytes: 32,
            ..Default::default()
        },
    )?;
    assert!(
        bounded
            .observe_admission_profiles(&"installation".try_into()?, &input()?)
            .await
            .is_err()
    );
    assert_eq!(server.records.lock().await.len(), 2);
    Ok(())
}
