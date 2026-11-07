use super::*;
use bitrouter_ai::tenancy::{ordered::MAX_MANIFEST_BYTES, usage_evidence_v2::EvidenceBudgetV2};
use bitrouter_sdk::error::Error;

fn manifest() -> TestResult<Value> {
    Ok(
        json!({"schema_version":1,"installation_id":"installation","tenant_id":"tenant",
        "execution_revision":"revision","authority_revision":7,"route":route()?,"inbound_protocol":"chat_completions",
        "candidates":[{"candidate_index":255,"target":{"provider_id":"provider","model_id":"model",
            "account_id":"opaque-account","credential_id":"opaque-credential","credential_version":"slot-revision"},
            "credential":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"credential",
            "resource_id":"slot","revision_id":"slot-revision"}}]}),
    )
}
async fn read_manifest(
    client: &TenancyClient,
) -> TestResult<bitrouter_ai::tenancy::ordered::CandidateManifestV1> {
    Ok(client
        .candidate_manifest(
            &"installation".try_into()?,
            &"tenant".try_into()?,
            &"revision".try_into()?,
            &route()?,
            GenerationProtocol::ChatCompletions,
        )
        .await?)
}
fn raw_json(value: String) -> TestResult<Response> {
    Ok(Response::builder()
        .header("content-type", "application/json")
        .body(HttpBody::from(value))?)
}

#[tokio::test]
async fn explicit_control_paths_bind_installation_and_preserve_all_v2_attempts() -> TestResult {
    for (version, wire) in versions() {
        let mut replies = vec![
            Some(server::json(serde_json::to_value(capabilities(64)?)?)?),
            Some(server::json(manifest()?)?),
        ];
        for count in [1, 8, 9, 64] {
            let receipt = receipt(count)?;
            replies.push(Some(server::json(serde_json::to_value(&receipt)?)?));
            replies.push(Some(server::json(serde_json::to_value(evidence(
                &receipt,
            )?)?)?));
        }
        let server = server::serve(wire, replies).await?;
        let client = control_client(&server, version)?;
        let caps = client.ordered_capabilities().await?;
        assert_eq!(caps.contract_version, 2);
        assert_eq!(caps.attempt_limit, 64);
        assert_eq!(caps.authority_revision.get(), 7);
        let manifest = read_manifest(&client).await?;
        assert_eq!(manifest.candidates[0].candidate_index, 255);
        for count in [1, 8, 9, 64] {
            let result = client
                .execution_v2(
                    &caps.installation_id,
                    &"tenant".try_into()?,
                    &"execution".try_into()?,
                )
                .await?;
            assert_eq!(result.attempts.len(), count);
            assert_eq!(result.accepted_selection.candidate_indices.len(), count);
            let evidence = client.usage_evidence_v2(&result).await?;
            assert_eq!(evidence.attempts.len(), count);
            evidence.validate_against(&result)?;
        }
        let records = server.records.lock().await;
        assert_eq!(records.len(), 10);
        for (index, record) in records.iter().enumerate() {
            assert_eq!(record.version, wire);
            assert_eq!(record.connection, records[0].connection);
            assert_eq!(record.method, "GET");
            assert!(record.body.is_empty());
            assert_eq!(record.headers["authorization"], "Bearer operator-token");
            assert_eq!(record.headers.get_all("authorization").iter().count(), 1);
            match index {
                0 => {
                    assert_eq!(record.path, "/control/v1/tenancy/ordered/capabilities");
                    assert!(!record.headers.contains_key("bitrouter-installation-id"));
                }
                1 => assert_eq!(
                    record.path,
                    "/control/v1/tenancy/tenants/tenant/configurations/revision/candidates?route_owner_kind=tenant&route_owner_id=tenant&route_id=route&route_revision=route-revision&inbound_protocol=chat_completions"
                ),
                _ if index % 2 == 0 => assert_eq!(
                    record.path,
                    "/control/v1/tenancy/tenants/tenant/executions/execution/v2"
                ),
                _ => assert_eq!(
                    record.path,
                    "/control/v1/tenancy/tenants/tenant/executions/execution/usage-evidence/v2"
                ),
            }
            if index > 0 {
                assert_eq!(record.headers["bitrouter-installation-id"], "installation");
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn manifest_queries_preserve_owner_and_protocol_roots() -> TestResult {
    use bitrouter_ai::tenancy::authority::OwnerKind;

    for (version, wire) in versions() {
        let mut cases = Vec::new();
        let mut replies = Vec::new();
        for owner in [OwnerKind::Tenant, OwnerKind::OperatorPool] {
            for (protocol, name) in [
                (GenerationProtocol::ChatCompletions, "chat_completions"),
                (GenerationProtocol::Messages, "messages"),
                (GenerationProtocol::Responses, "responses"),
            ] {
                let mut route = route()?;
                route.owner_kind = owner;
                // A tenant-owned root must belong to the queried tenant. The
                // shared pool case independently exercises encoded owner IDs.
                route.owner_id = match owner {
                    OwnerKind::Tenant => "tenant",
                    OwnerKind::OperatorPool => "owner:one",
                }
                .try_into()?;
                route.resource_id = "route:one".try_into()?;
                let mut value = manifest()?;
                value["route"] = serde_json::to_value(&route)?;
                value["inbound_protocol"] = json!(name);
                replies.push(Some(server::json(value)?));
                cases.push((route, protocol, name));
            }
        }
        let server = server::serve(wire, replies).await?;
        let client = control_client(&server, version)?;
        for (route, protocol, _) in &cases {
            let result = client
                .candidate_manifest(
                    &"installation".try_into()?,
                    &"tenant".try_into()?,
                    &"revision".try_into()?,
                    route,
                    *protocol,
                )
                .await?;
            assert_eq!(result.route, *route);
            assert_eq!(result.inbound_protocol, *protocol);
        }
        let records = server.records.lock().await;
        assert_eq!(records.len(), cases.len());
        for (record, (route, _, name)) in records.iter().zip(cases) {
            let url = url::Url::parse(&format!("http://localhost{}", record.path))?;
            let query = url.query_pairs().into_owned().collect::<Vec<_>>();
            let owner = if route.owner_kind == OwnerKind::Tenant {
                "tenant"
            } else {
                "operator_pool"
            };
            assert_eq!(
                query,
                vec![
                    ("route_owner_kind".into(), owner.into()),
                    ("route_owner_id".into(), route.owner_id.as_str().into()),
                    ("route_id".into(), "route:one".into()),
                    ("route_revision".into(), "route-revision".into()),
                    ("inbound_protocol".into(), name.into()),
                ]
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn manifest_rejects_wrong_bindings_versions_duplicate_fields_and_secret_extensions()
-> TestResult {
    for (version, wire) in versions() {
        let mut replies = Vec::new();
        for (pointer, value) in [
            ("/schema_version", json!(2)),
            ("/installation_id", json!("other")),
            ("/tenant_id", json!("other")),
            ("/execution_revision", json!("other")),
            ("/route/owner_kind", json!("operator_pool")),
            ("/route/owner_id", json!("other")),
            ("/route/resource_id", json!("other")),
            ("/route/revision_id", json!("other")),
            ("/inbound_protocol", json!("messages")),
            ("/candidates/0/candidate_index", json!(256)),
            ("/candidates/0/credential/resource_kind", json!("route")),
        ] {
            let mut value_base = manifest()?;
            *value_base.pointer_mut(pointer).ok_or("fixture field")? = value;
            replies.push(Some(server::json(value_base)?));
        }
        let mut secret = manifest()?;
        secret["candidates"][0]["api_key"] = json!("manifest-secret-canary");
        replies.push(Some(server::json(secret)?));
        let original = manifest()?.to_string();
        replies.push(Some(raw_json(format!(
            "{{\"schema_version\":1,{}",
            &original[1..]
        ))?));
        let count = replies.len();
        let server = server::serve(wire, replies).await?;
        let client = control_client(&server, version)?;
        for _ in 0..count {
            let error = read_manifest(&client)
                .await
                .err()
                .ok_or("invalid manifest accepted")?;
            assert!(!format!("{error:?}").contains("manifest-secret-canary"));
        }
        assert_eq!(server.records.lock().await.len(), count);
    }
    Ok(())
}

#[tokio::test]
async fn discovery_refuses_unsupported_versions_and_errors_without_fallback_or_redirect()
-> TestResult {
    for (version, wire) in versions() {
        let mut old = serde_json::to_value(capabilities(8)?)?;
        old["contract_version"] = json!(1);
        let redirect = Response::builder()
            .status(307)
            .header("location", "/control/v1/tenancy/capabilities")
            .header("content-type", "application/json")
            .body(HttpBody::from(error_body().to_string()))?;
        let server = server::serve(
            wire,
            vec![
                Some(server::json(old)?),
                Some(json_status(409, error_body())?),
                Some(redirect),
                Some(server::json(serde_json::to_value(capabilities(8)?)?)?),
            ],
        )
        .await?;
        let client = control_client(&server, version)?;
        for _ in 0..3 {
            assert!(client.ordered_capabilities().await.is_err());
        }
        assert_eq!(client.ordered_capabilities().await?.attempt_limit, 8);
        let records = server.records.lock().await;
        assert_eq!(records.len(), 4);
        assert!(
            records
                .iter()
                .all(|record| record.path == "/control/v1/tenancy/ordered/capabilities")
        );
    }
    Ok(())
}

#[tokio::test]
async fn manifest_auth_refusal_and_local_validation_do_not_send_followups() -> TestResult {
    let mut forbidden = error_body();
    forbidden["code"] = json!("forbidden");
    let server = server::serve(Version::HTTP_11, vec![Some(json_status(403, forbidden)?)]).await?;
    let client = control_client(&server, HttpVersion::Http1Only)?;
    assert!(read_manifest(&client).await.is_err());
    let mut wrong = route()?;
    wrong.resource_kind = bitrouter_ai::tenancy::authority::ResourceKind::Credential;
    assert!(matches!(
        client
            .candidate_manifest(
                &"installation".try_into()?,
                &"tenant".try_into()?,
                &"revision".try_into()?,
                &wrong,
                GenerationProtocol::ChatCompletions
            )
            .await,
        Err(Error::Request(_))
    ));
    let mut invalid = receipt(1)?;
    invalid.accepted_selection.candidate_indices.clear();
    assert!(matches!(
        client.usage_evidence_v2(&invalid).await,
        Err(Error::Request(_))
    ));
    assert_eq!(server.records.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn receipt_and_evidence_require_exact_identity_and_selection() -> TestResult {
    for (version, wire) in versions() {
        let original = receipt(9)?;
        let mut replies = Vec::new();
        for key in ["installation_id", "tenant_id", "execution_id"] {
            let mut value = serde_json::to_value(&original)?;
            value[key] = json!("other");
            replies.push(Some(server::json(value)?));
        }
        for pointer in [
            "/installation_id",
            "/tenant_id",
            "/execution_id",
            "/accepted_selection/route/revision_id",
            "/accepted_selection/inbound_protocol",
        ] {
            let mut value = serde_json::to_value(evidence(&original)?)?;
            *value.pointer_mut(pointer).ok_or("fixture field")? =
                if pointer.ends_with("inbound_protocol") {
                    json!("messages")
                } else {
                    json!("other")
                };
            replies.push(Some(server::json(value)?));
        }
        let mut value = serde_json::to_value(evidence(&original)?)?;
        value["attempts"]
            .as_array_mut()
            .ok_or("attempts")?
            .swap(0, 1);
        replies.push(Some(server::json(value)?));
        let server = server::serve(wire, replies).await?;
        let client = control_client(&server, version)?;
        for _ in 0..3 {
            assert!(
                client
                    .execution_v2(
                        &original.installation_id,
                        &original.tenant_id,
                        &original.execution_id
                    )
                    .await
                    .is_err()
            );
        }
        for _ in 0..6 {
            assert!(client.usage_evidence_v2(&original).await.is_err());
        }
        assert_eq!(server.records.lock().await.len(), 9);
    }
    Ok(())
}

#[tokio::test]
async fn explicit_v1_v2_receipt_and_evidence_reads_never_convert_formats() -> TestResult {
    for (version, wire) in versions() {
        let receipt = receipt(1)?;
        let old_receipt = json!({"installation_id":"installation","tenant_id":"tenant","principal_id":"principal",
            "execution_id":"execution","correlation_id":"correlation","execution_revision":"revision",
            "state":"unknown","outcome":"unknown","attempts":[],"usage":unknown_usage(),"recorded_at":"2026-11-02T00:00:00Z"});
        let old_evidence = json!({"schema_version":1,"installation_id":"installation","tenant_id":"tenant","execution_id":"execution",
            "parent_execution_id":null,"child_execution_ids":[],"capture":"not_captured","extractor_version":1,"estimation":"not_captured",
            "estimate":null,"attempts":[],"frames":[],"truncated":false,"dropped_frames":0});
        serde_json::from_value::<bitrouter_ai::tenancy::execution::ExecutionReceipt>(
            old_receipt.clone(),
        )?
        .validate()?;
        serde_json::from_value::<bitrouter_ai::tenancy::usage_evidence::ExecutionUsageEvidenceV1>(
            old_evidence.clone(),
        )?
        .validate()?;
        let server = server::serve(
            wire,
            vec![
                Some(server::json(old_receipt)?),
                Some(server::json(serde_json::to_value(&receipt)?)?),
                Some(server::json(old_evidence)?),
                Some(server::json(serde_json::to_value(evidence(&receipt)?)?)?),
            ],
        )
        .await?;
        let client = control_client(&server, version)?;
        assert!(
            client
                .execution_v2(
                    &receipt.installation_id,
                    &receipt.tenant_id,
                    &receipt.execution_id
                )
                .await
                .is_err()
        );
        assert!(
            client
                .execution(
                    &receipt.installation_id,
                    &receipt.tenant_id,
                    &receipt.execution_id
                )
                .await
                .is_err()
        );
        assert!(client.usage_evidence_v2(&receipt).await.is_err());
        assert!(
            client
                .usage_evidence(
                    &receipt.installation_id,
                    &receipt.tenant_id,
                    &receipt.execution_id
                )
                .await
                .is_err()
        );
        assert_eq!(server.records.lock().await.len(), 4);
    }
    Ok(())
}

fn assert_limit<T>(result: bitrouter_sdk::error::Result<T>) -> TestResult {
    let Err(Error::Tenancy(failure)) = result else {
        return Err("expected bounded failure".into());
    };
    assert!(matches!(failure.body_error, Some(Error::Limit)));
    Ok(())
}

#[tokio::test]
async fn raw_response_budgets_include_whitespace_and_follow_accepted_count() -> TestResult {
    for (version, wire) in versions() {
        let mut replies = vec![
            Some(raw_json(format!(
                "{}{}",
                manifest()?,
                " ".repeat(MAX_MANIFEST_BYTES)
            ))?),
            Some(raw_json(format!(
                "{}{}",
                serde_json::to_string(&receipt(1)?)?,
                " ".repeat(1024 * 1024)
            ))?),
        ];
        for count in [1, 8, 9, 64] {
            let receipt = receipt(count)?;
            let budget = EvidenceBudgetV2::for_attempts(usize::from(count))?;
            let mut body = serde_json::to_string(&evidence(&receipt)?)?;
            body.push_str(&" ".repeat(budget.max_bytes - body.len()));
            replies.push(Some(raw_json(body.clone())?));
            body.push(' ');
            replies.push(Some(raw_json(body)?));
        }
        let server = server::serve(wire, replies).await?;
        let client = control_client(&server, version)?;
        assert_limit(
            client
                .candidate_manifest(
                    &"installation".try_into()?,
                    &"tenant".try_into()?,
                    &"revision".try_into()?,
                    &route()?,
                    GenerationProtocol::ChatCompletions,
                )
                .await,
        )?;
        let first = receipt(1)?;
        assert_limit(
            client
                .execution_v2(
                    &first.installation_id,
                    &first.tenant_id,
                    &first.execution_id,
                )
                .await,
        )?;
        for count in [1, 8, 9, 64] {
            let receipt = receipt(count)?;
            assert_eq!(
                client.usage_evidence_v2(&receipt).await?.attempts.len(),
                usize::from(count)
            );
            assert_limit(client.usage_evidence_v2(&receipt).await)?;
        }
        assert_eq!(server.records.lock().await.len(), 10);
    }
    Ok(())
}
