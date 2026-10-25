mod bounds;
mod failures;
mod identity;

use super::*;

#[tokio::test]
async fn seven_routes_preserve_dtos_explicit_namespace_and_singleton_headers() -> TestResult {
    for (wire, version) in protocols() {
        let replies = vec![
            json!({"eval_id":"eval-1","outcome":"inserted"}),
            json!([subject_value("eval-1")]),
            subject_value("eval-1"),
            receipt(),
            snapshot(),
            snapshot(),
            status(1),
        ]
        .into_iter()
        .map(|value| reply(200, value).map(Some))
        .collect::<TestResult<Vec<_>>>()?;
        let server = server::serve(wire, replies).await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let subject = subject()?;
        let result = result()?;
        let freeze = FreezeRequest {
            frozen_at: Some(TIME.into()),
        };
        assert_eq!(
            client
                .put_eval_subject(&installation, &tenant, &subject)
                .await?,
            serde_json::from_value::<bitrouter_ai::eval::PutSubjectReport>(
                json!({"eval_id":"eval-1","outcome":"inserted"})
            )?,
        );
        assert_eq!(
            client.eval_subjects(&installation, &tenant).await?,
            vec![subject.clone()]
        );
        assert_eq!(
            client
                .eval_subject(&installation, &tenant, "eval-1")
                .await?,
            subject
        );
        assert_eq!(
            client
                .submit_eval_result(&installation, &tenant, &result)
                .await?,
            serde_json::from_value::<bitrouter_ai::eval::AdmissionOutcome>(receipt())?
        );
        assert_eq!(
            client
                .freeze_eval_snapshot(&installation, &tenant, &freeze)
                .await?,
            serde_json::from_value::<bitrouter_ai::eval::EvalSnapshot>(snapshot())?
        );
        assert_eq!(
            client
                .clone()
                .eval_snapshot(&installation, &tenant, &digest('e'))
                .await?,
            serde_json::from_value::<bitrouter_ai::eval::EvalSnapshot>(snapshot())?
        );
        assert_eq!(
            client.eval_status(&installation, &tenant).await?,
            serde_json::from_value::<bitrouter_ai::eval::StatusReport>(status(1))?
        );

        let records = server.records.lock().await;
        assert_eq!(records.len(), 7);
        let expected = [
            (
                "POST",
                "subjects".to_owned(),
                Some(serde_json::to_value(&subject)?),
            ),
            ("GET", "subjects".to_owned(), None),
            ("GET", "subjects/eval-1".to_owned(), None),
            (
                "POST",
                "results".to_owned(),
                Some(serde_json::to_value(&result)?),
            ),
            (
                "POST",
                "snapshots".to_owned(),
                Some(serde_json::to_value(&freeze)?),
            ),
            ("GET", format!("snapshots/{}", digest('e')), None),
            ("GET", "status".to_owned(), None),
        ];
        for (record, (method, suffix, body)) in records.iter().zip(expected) {
            assert_eq!(record.method, method);
            assert_eq!(
                record.path,
                format!("/control/v1/tenants/tenant/evals/{suffix}")
            );
            assert_eq!(record.version, wire);
            assert_eq!(record.connection, records[0].connection);
            assert_eq!(
                record
                    .headers
                    .get_all("bitrouter-installation-id")
                    .iter()
                    .count(),
                1
            );
            assert_eq!(record.headers["bitrouter-installation-id"], "installation");
            assert_eq!(
                record.headers.get_all(header::AUTHORIZATION).iter().count(),
                1
            );
            assert_eq!(
                record.headers[header::AUTHORIZATION].to_str()?,
                format!("Bearer {TOKEN}")
            );
            assert_eq!(record.headers[header::ACCEPT], "application/json");
            assert_eq!(record.headers[header::CACHE_CONTROL], "no-store");
            assert!(!record.headers.contains_key("x-api-key"));
            if let Some(body) = body {
                assert_eq!(record.headers[header::CONTENT_TYPE], "application/json");
                assert_eq!(serde_json::from_slice::<Value>(&record.body)?, body);
                assert!(body.get("installation_id").is_none());
                assert!(body.get("operation_id").is_none());
                assert!(body.get("tenant_id").is_none());
            } else {
                assert!(record.body.is_empty());
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn logical_ids_are_one_segment_and_invalid_path_ids_do_not_dispatch() -> TestResult {
    for (wire, version) in protocols() {
        let id = "segment/with?query#fragment%2e";
        let server = server::serve(wire, vec![Some(reply(200, subject_value(id))?)]).await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let long = "x".repeat(513);
        for invalid in ["", " ", ".", "..", "bad\nline", long.as_str()] {
            assert!(matches!(
                client.eval_subject(&installation, &tenant, invalid).await,
                Err(Error::Request(_))
            ));
            assert!(matches!(
                client.eval_snapshot(&installation, &tenant, invalid).await,
                Err(Error::Request(_))
            ));
        }
        assert!(server.records.lock().await.is_empty());
        assert_eq!(
            client
                .eval_subject(&installation, &tenant, id)
                .await?
                .eval_id,
            id
        );
        let records = server.records.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].path,
            "/control/v1/tenants/tenant/evals/subjects/segment%2Fwith%3Fquery%23fragment%252e"
        );
    }
    Ok(())
}

#[tokio::test]
async fn reads_stay_fresh_and_each_call_uses_its_own_namespace() -> TestResult {
    for (wire, version) in protocols() {
        let server = server::serve(
            wire,
            vec![
                Some(reply(200, status(1))?),
                Some(reply(200, status(2))?),
                Some(reply(200, status(3))?),
            ],
        )
        .await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        assert_eq!(
            client.eval_status(&installation, &tenant).await?.subjects,
            1
        );
        assert_eq!(
            client.eval_status(&installation, &tenant).await?.subjects,
            2
        );
        assert_eq!(
            client
                .clone()
                .eval_status(&"installation-2".try_into()?, &"tenant-2".try_into()?)
                .await?
                .subjects,
            3
        );
        let records = server.records.lock().await;
        assert_eq!(records.len(), 3);
        assert_eq!(records[2].path, "/control/v1/tenants/tenant-2/evals/status");
        assert_eq!(
            records[2].headers["bitrouter-installation-id"],
            "installation-2"
        );
        assert_eq!(
            records[2]
                .headers
                .get_all("bitrouter-installation-id")
                .iter()
                .count(),
            1
        );
    }
    Ok(())
}
