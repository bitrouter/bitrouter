use super::*;

#[tokio::test]
async fn model_policy_subject_preserves_resource_revision_and_literal_name() -> TestResult {
    for (wire, version) in protocols() {
        let mut value = subject_value("eval-1");
        value["identity"] = json!({
            "profile":"tenant_model_policy","installation_id":"installation","tenant_id":"tenant",
            "configuration_revision":"configuration-r1",
            "policy":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"policy","resource_id":"policy-a","revision_id":"policy-r1"},
            "name":"auto:cost","source_digest":digest('a'),"classifier_digest":digest('c')
        });
        let server = server::serve(
            wire,
            vec![
                Some(reply(
                    200,
                    json!({"eval_id":"eval-1","outcome":"inserted"}),
                )?),
                Some(reply(200, value.clone())?),
            ],
        )
        .await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let subject: EvalSubject = serde_json::from_value(value.clone())?;
        client
            .put_eval_subject(&installation, &tenant, &subject)
            .await?;
        assert_eq!(
            client
                .eval_subject(&installation, &tenant, "eval-1")
                .await?,
            subject
        );
        let records = server.records.lock().await;
        assert_eq!(records.len(), 2);
        assert_eq!(
            serde_json::from_slice::<Value>(&records[0].body)?["identity"],
            value["identity"]
        );
    }
    Ok(())
}

#[tokio::test]
async fn subject_responses_refuse_old_versions_foreign_namespaces_and_gateway_profiles()
-> TestResult {
    for (wire, version) in protocols() {
        let mut old = subject_value("eval-1");
        old["schema_version"] = 1.into();
        let mut foreign = subject_value("eval-1");
        foreign["identity"]["tenant_id"] = "other-tenant".into();
        let mut gateway = subject_value("eval-1");
        gateway["identity"] = json!({"profile":"gateway_generic"});
        let server = server::serve(
            wire,
            vec![
                Some(reply(200, old)?),
                Some(reply(200, foreign)?),
                Some(reply(200, gateway)?),
            ],
        )
        .await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        for _ in 0..3 {
            let error = client
                .eval_subject(&installation, &tenant, "eval-1")
                .await
                .err()
                .ok_or("invalid subject accepted")?;
            assert!(matches!(
                failure(error)?.body_error,
                Some(Error::Response(_))
            ));
        }
        assert_eq!(server.records.lock().await.len(), 3);
    }
    Ok(())
}

#[tokio::test]
async fn subject_post_refuses_foreign_or_gateway_identity_before_dispatch() -> TestResult {
    for (wire, version) in protocols() {
        let server = server::serve(wire, Vec::new()).await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let mut input = subject()?;
        input.identity = bitrouter_ai::eval::EvalSubjectIdentity::GatewayGeneric {};
        assert!(matches!(
            client
                .put_eval_subject(&installation, &tenant, &input)
                .await,
            Err(Error::Request(_))
        ));
        input.identity = bitrouter_ai::eval::EvalSubjectIdentity::TenantGeneric {
            installation_id: installation.clone(),
            tenant_id: "foreign".try_into()?,
            execution: None,
        };
        assert!(matches!(
            client
                .put_eval_subject(&installation, &tenant, &input)
                .await,
            Err(Error::Request(_))
        ));
        assert!(server.records.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn explicit_freeze_timestamp_is_preserved_and_mismatch_is_unknown() -> TestResult {
    for (wire, version) in protocols() {
        let explicit = "2026-10-25T00:00:00.000+00:00";
        let mut exact = snapshot();
        exact["frozen_at"] = explicit.into();
        let server = server::serve(
            wire,
            vec![Some(reply(200, exact)?), Some(reply(200, snapshot())?)],
        )
        .await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let request = FreezeRequest {
            frozen_at: Some(explicit.into()),
        };
        let returned = client
            .freeze_eval_snapshot(&installation, &tenant, &request)
            .await?;
        assert_eq!(returned.frozen_at, explicit);
        // The second reply names the same instant, but a different immutable
        // label. The SDK neither normalizes it nor computes a private root.
        let error = client
            .freeze_eval_snapshot(&installation, &tenant, &request)
            .await
            .err()
            .ok_or("freeze accepted a changed timestamp label")?;
        let failed = failure(unknown(error)?)?;
        assert_eq!(failed.status, 200);
        assert_eq!(failed.headers[header::CACHE_CONTROL], "no-store");
        assert!(matches!(failed.body_error, Some(Error::Response(_))));
        let records = server.records.lock().await;
        assert_eq!(records.len(), 2);
        for record in records.iter() {
            let body: Value = serde_json::from_slice(&record.body)?;
            assert_eq!(body["frozen_at"], explicit);
        }
    }
    Ok(())
}

#[tokio::test]
async fn result_identity_is_opaque_and_is_not_inferred_from_request_or_private_hash_rules()
-> TestResult {
    for (wire, version) in protocols() {
        let mut receipt = receipt();
        receipt["result_id"] = "opaque-result-identity".into();
        receipt["duplicate"] = true.into();
        let server = server::serve(wire, vec![Some(reply(200, receipt)?)]).await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let input = result()?;
        let output = client
            .submit_eval_result(&installation, &tenant, &input)
            .await?;
        assert_eq!(output.result_id, "opaque-result-identity");
        assert_ne!(output.result_id, input.eval_id);
        assert_ne!(output.result_id, input.idempotency_key);
        assert!(output.duplicate);
        assert_eq!(output.status, bitrouter_ai::eval::AdmissionStatus::HeldOut);
        let records = server.records.lock().await;
        assert_eq!(records.len(), 1);
        let body: Value = serde_json::from_slice(&records[0].body)?;
        assert_eq!(body["eval_id"], input.eval_id);
        assert_eq!(body["idempotency_key"], input.idempotency_key);
        assert!(body.get("operation_id").is_none());
    }
    Ok(())
}

#[tokio::test]
async fn snapshot_parent_result_and_root_identifiers_remain_opaque() -> TestResult {
    for (wire, version) in protocols() {
        let mut manifest = snapshot();
        manifest["evidence_root"] = "opaque-root".into();
        manifest["entries"][0]["result_id"] = "opaque-result".into();
        manifest["entries"][0]["content_digest"] = "opaque-content".into();
        manifest["entries"][0]["subject_content_digest"] = "opaque-parent-content".into();
        let server = server::serve(wire, vec![Some(reply(200, manifest)?)]).await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let output = client
            .eval_snapshot(&installation, &tenant, "opaque-root")
            .await?;
        assert_eq!(output.evidence_root, "opaque-root");
        assert_eq!(output.entries.len(), 1);
        assert_eq!(output.entries[0].result_id, "opaque-result");
        assert_eq!(output.entries[0].eval_id, "eval-1");
        assert_eq!(
            output.entries[0].subject_content_digest,
            "opaque-parent-content"
        );
    }
    Ok(())
}
