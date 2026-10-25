use super::*;

#[tokio::test]
async fn typed_post_dispatch_denials_are_unknown_for_all_three_mutations() -> TestResult {
    for (wire, version) in protocols() {
        for (status, code) in [
            (401, "unauthorized"),
            (403, "forbidden"),
            (503, "authority_unavailable"),
        ] {
            for mutation in [Mutation::Subject, Mutation::Result, Mutation::Freeze] {
                let mut response = reply(status, rejection(code))?;
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, "Bearer".parse()?);
                let server = server::serve(wire, vec![Some(response)]).await?;
                let client = client(&server, version)?;
                let (installation, tenant) = namespace()?;
                let error = mutation
                    .send(&client, &installation, &tenant)
                    .await?
                    .err()
                    .ok_or("mutation denial unexpectedly succeeded")?;
                let failure = failure(unknown(error)?)?;
                assert_eq!(failure.status, status);
                assert_eq!(failure.headers[header::CACHE_CONTROL], "no-store");
                assert_eq!(failure.headers[header::WWW_AUTHENTICATE], "Bearer");
                assert!(failure.body_error.is_none());
                let body = failure.body.ok_or("missing typed tenancy error")?;
                assert_eq!(serde_json::to_value(body.code)?, code);
                assert_eq!(
                    body.dispatch_state,
                    bitrouter_ai::tenancy::operation::DispatchState::NotStarted
                );
                assert!(body.operation_id.is_none());
                assert!(body.execution_id.is_none());
                assert_eq!(server.records.lock().await.len(), 1);
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn read_failure_retains_typed_status_without_unknown_mutation_wrapper() -> TestResult {
    for (wire, version) in protocols() {
        let server = server::serve(wire, vec![Some(reply(403, rejection("forbidden"))?)]).await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let error = client
            .eval_status(&installation, &tenant)
            .await
            .err()
            .ok_or("read unexpectedly succeeded")?;
        let failure = failure(error)?;
        assert_eq!(failure.status, 403);
        assert_eq!(failure.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(failure.body.ok_or("missing error")?.message, TOKEN);
    }
    Ok(())
}

#[tokio::test]
async fn disconnect_after_full_post_is_unknown_and_never_retried() -> TestResult {
    for (wire, version) in protocols() {
        for mutation in [Mutation::Subject, Mutation::Result, Mutation::Freeze] {
            let success = match mutation {
                Mutation::Subject => json!({"eval_id":"eval-1","outcome":"inserted"}),
                Mutation::Result => receipt(),
                Mutation::Freeze => snapshot(),
            };
            let server = server::serve(wire, vec![None, Some(reply(200, success)?)]).await?;
            let client = client(&server, version)?;
            let (installation, tenant) = namespace()?;
            let error = mutation
                .send(&client, &installation, &tenant)
                .await?
                .err()
                .ok_or("lost response unexpectedly succeeded")?;
            assert!(matches!(unknown(error)?, Error::Transport(_)));
            let records = server.records.lock().await;
            assert_eq!(records.len(), 1, "{wire:?} {mutation:?} must not replay");
            assert_eq!(records[0].method, "POST");
            assert!(!records[0].body.is_empty());
            assert_eq!(
                records[0].headers["bitrouter-installation-id"],
                "installation"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn redirects_never_forward_bearer_and_diagnostics_redact_response_material() -> TestResult {
    for (wire, version) in protocols() {
        let destination = server::serve(wire, vec![Some(reply(200, status(1))?)]).await?;
        let mut redirect = reply(307, rejection("authority_unavailable"))?;
        redirect.headers_mut().insert(
            header::LOCATION,
            format!("{}/?secret={TOKEN}", destination.endpoint).parse()?,
        );
        redirect
            .headers_mut()
            .insert("x-response-canary", TOKEN.parse()?);
        let server = server::serve(wire, vec![Some(redirect)]).await?;
        let client = client(&server, version)?;
        assert!(!format!("{client:?}").contains(TOKEN));
        let (installation, tenant) = namespace()?;
        let error = client
            .freeze_eval_snapshot(&installation, &tenant, &FreezeRequest::default())
            .await
            .err()
            .ok_or("redirect followed")?;
        let failure = failure(unknown(error)?)?;
        assert_eq!(failure.status, 307);
        assert!(failure.headers[header::LOCATION].to_str()?.contains(TOKEN));
        assert_eq!(failure.headers["x-response-canary"], TOKEN);
        assert!(destination.records.lock().await.is_empty());
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn wrong_subject_or_root_is_rejected_and_mutation_mismatch_stays_unknown() -> TestResult {
    for (wire, version) in protocols() {
        let server = server::serve(
            wire,
            vec![
                Some(reply(
                    200,
                    json!({"eval_id":"wrong-eval","outcome":"inserted"}),
                )?),
                Some(reply(200, subject_value("wrong-eval"))?),
                Some(reply(200, snapshot())?),
            ],
        )
        .await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let error = client
            .put_eval_subject(&installation, &tenant, &subject()?)
            .await
            .err()
            .ok_or("wrong subject accepted")?;
        let mismatch = failure(unknown(error)?)?;
        assert_eq!(mismatch.status, 200);
        assert_eq!(mismatch.headers[header::CACHE_CONTROL], "no-store");
        assert!(matches!(mismatch.body_error, Some(Error::Response(_))));
        let error = client
            .eval_subject(&installation, &tenant, "eval-1")
            .await
            .err()
            .ok_or("wrong subject read accepted")?;
        assert!(matches!(
            failure(error)?.body_error,
            Some(Error::Response(_))
        ));
        let error = client
            .eval_snapshot(&installation, &tenant, &digest('a'))
            .await
            .err()
            .ok_or("wrong snapshot root accepted")?;
        assert!(matches!(
            failure(error)?.body_error,
            Some(Error::Response(_))
        ));
        assert_eq!(server.records.lock().await.len(), 3);
    }
    Ok(())
}

#[tokio::test]
async fn malformed_identity_in_success_or_error_cannot_be_treated_as_a_receipt() -> TestResult {
    for (wire, version) in protocols() {
        let mut bad_result = receipt();
        bad_result["result_id"] = "".into();
        let mut bad_manifest = snapshot();
        bad_manifest["entries"][0]["eval_id"] = " ".into();
        let mut bad_parent = subject_value("eval-1");
        bad_parent["subject_id"] = "".into();
        let mut fake_operation = rejection("forbidden");
        fake_operation["operation_id"] = "invented-management-id".into();
        let server = server::serve(
            wire,
            vec![
                Some(reply(200, bad_result)?),
                Some(reply(200, bad_manifest)?),
                Some(reply(200, bad_parent)?),
                Some(reply(403, fake_operation)?),
            ],
        )
        .await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        for mutation in [Mutation::Result, Mutation::Freeze] {
            let error = mutation
                .send(&client, &installation, &tenant)
                .await?
                .err()
                .ok_or("invalid identity accepted")?;
            let failure = failure(unknown(error)?)?;
            assert_eq!(failure.status, 200);
            assert!(matches!(failure.body_error, Some(Error::Response(_))));
        }
        let error = client
            .eval_subject(&installation, &tenant, "eval-1")
            .await
            .err()
            .ok_or("invalid parent accepted")?;
        assert!(matches!(
            failure(error)?.body_error,
            Some(Error::Response(_))
        ));
        let error = client
            .submit_eval_result(&installation, &tenant, &result()?)
            .await
            .err()
            .ok_or("bad error accepted")?;
        let failure = failure(unknown(error)?)?;
        assert_eq!(failure.status, 403);
        assert!(failure.body.is_none());
        assert!(matches!(failure.body_error, Some(Error::Response(_))));
    }
    Ok(())
}

#[tokio::test]
async fn duplicate_known_fields_in_success_are_not_collapsed_before_decoding() -> TestResult {
    for (wire, version) in protocols() {
        let body =
            format!("{{\"eval_id\":\"eval-1\",\"eval_id\":\"{TOKEN}\",\"outcome\":\"inserted\"}}");
        let server = server::serve(wire, vec![Some(response(Body::from(body))?)]).await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let error = client
            .put_eval_subject(&installation, &tenant, &subject()?)
            .await
            .err()
            .ok_or("duplicate fields accepted")?;
        let failure = failure(unknown(error)?)?;
        assert_eq!(failure.status, 200);
        assert!(matches!(failure.body_error, Some(Error::Response(_))));
    }
    Ok(())
}
