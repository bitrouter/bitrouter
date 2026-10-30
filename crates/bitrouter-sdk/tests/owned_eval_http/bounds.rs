use bytes::Bytes;

use super::*;

#[tokio::test]
async fn oversized_inputs_are_rejected_before_any_request_is_sent() -> TestResult {
    for (wire, version) in protocols() {
        let server = server::serve(wire, Vec::new()).await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        let mut subject = subject()?;
        subject.cohort = Some("x".repeat(1024 * 1024));
        assert!(matches!(
            client
                .put_eval_subject(&installation, &tenant, &subject)
                .await,
            Err(Error::Request(_))
        ));
        let mut result = result()?;
        result.evidence_refs.push("x".repeat(1024 * 1024));
        assert!(matches!(
            client
                .submit_eval_result(&installation, &tenant, &result)
                .await,
            Err(Error::Request(_))
        ));
        // JSON escaping counts against the wire limit, not only source length.
        let freeze = FreezeRequest {
            frozen_at: Some("\n".repeat(2048)),
        };
        assert!(matches!(
            client
                .freeze_eval_snapshot(&installation, &tenant, &freeze)
                .await,
            Err(Error::Request(_))
        ));
        assert!(server.records.lock().await.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn configured_response_limit_applies_to_known_and_streamed_lengths() -> TestResult {
    for (wire, version) in protocols() {
        let bytes = serde_json::to_vec(&snapshot())?;
        let streamed = Body::from_stream(futures_util::stream::iter(vec![
            Ok::<Bytes, std::io::Error>(Bytes::copy_from_slice(&bytes[..40])),
            Ok(Bytes::copy_from_slice(&bytes[40..])),
        ]));
        let server = server::serve(
            wire,
            vec![Some(reply(200, snapshot())?), Some(response(streamed)?)],
        )
        .await?;
        let client = TenancyClient::with_options(
            &server.endpoint,
            TOKEN,
            TenancyOptions {
                http_version: version,
                max_response_bytes: 64,
                ..Default::default()
            },
        )?;
        let (installation, tenant) = namespace()?;
        let error = client
            .eval_snapshot(&installation, &tenant, &digest('e'))
            .await
            .err()
            .ok_or("known oversized response accepted")?;
        assert!(matches!(failure(error)?.body_error, Some(Error::Limit)));
        let error = client
            .freeze_eval_snapshot(&installation, &tenant, &FreezeRequest::default())
            .await
            .err()
            .ok_or("streamed oversized response accepted")?;
        let failure = failure(unknown(error)?)?;
        assert_eq!(failure.status, 200);
        assert_eq!(failure.headers[header::CACHE_CONTROL], "no-store");
        assert!(matches!(failure.body_error, Some(Error::Limit)));
        assert_eq!(server.records.lock().await.len(), 2);
    }
    Ok(())
}

#[tokio::test]
async fn owned_route_response_ceiling_applies_even_if_transport_limit_is_larger() -> TestResult {
    for (wire, version) in protocols() {
        let body =
            Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
        let mut reply = response(body)?;
        reply.headers_mut().insert(
            header::CONTENT_LENGTH,
            (8 * 1024 * 1024 + 1).to_string().parse()?,
        );
        let server = server::serve(wire, vec![Some(reply)]).await?;
        let client = TenancyClient::with_options(
            &server.endpoint,
            TOKEN,
            TenancyOptions {
                http_version: version,
                max_response_bytes: 16 * 1024 * 1024,
                ..Default::default()
            },
        )?;
        let (installation, tenant) = namespace()?;
        let error = tokio::time::timeout(
            Duration::from_secs(2),
            client.eval_status(&installation, &tenant),
        )
        .await?
        .err()
        .ok_or("ceiling not enforced")?;
        assert!(matches!(failure(error)?.body_error, Some(Error::Limit)));
    }
    Ok(())
}

#[tokio::test]
async fn delayed_mutation_body_is_bounded_and_retains_unknown_outcome() -> TestResult {
    for (wire, version) in protocols() {
        for mutation in [Mutation::Subject, Mutation::Result, Mutation::Freeze] {
            let body =
                Body::from_stream(futures_util::stream::pending::<Result<Bytes, std::io::Error>>());
            let server = server::serve(wire, vec![Some(response(body)?)]).await?;
            let client = TenancyClient::with_options(
                &server.endpoint,
                TOKEN,
                TenancyOptions {
                    http_version: version,
                    request_timeout: Duration::from_secs(1),
                    ..Default::default()
                },
            )?;
            let (installation, tenant) = namespace()?;
            let error = tokio::time::timeout(
                Duration::from_secs(3),
                mutation.send(&client, &installation, &tenant),
            )
            .await??
            .err()
            .ok_or("stalled response succeeded")?;
            let failure = failure(unknown(error)?)?;
            assert_eq!(failure.status, 200);
            assert!(matches!(failure.body_error, Some(Error::Transport(_))));
            assert_eq!(server.records.lock().await.len(), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn unreadable_error_body_preserves_status_headers_and_unknown_mutation() -> TestResult {
    for (wire, version) in protocols() {
        let mut oversized = response(Body::from("x".repeat(1025)))?;
        *oversized.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        oversized
            .headers_mut()
            .insert(header::RETRY_AFTER, "3".parse()?);
        let server = server::serve(wire, vec![Some(oversized)]).await?;
        let client = TenancyClient::with_options(
            &server.endpoint,
            TOKEN,
            TenancyOptions {
                http_version: version,
                max_response_bytes: 1024,
                ..Default::default()
            },
        )?;
        let (installation, tenant) = namespace()?;
        let error = client
            .submit_eval_result(&installation, &tenant, &result()?)
            .await
            .err()
            .ok_or("oversized failure accepted")?;
        let failure = failure(unknown(error)?)?;
        assert_eq!(failure.status, 503);
        assert_eq!(failure.headers[header::CACHE_CONTROL], "no-store");
        assert_eq!(failure.headers[header::RETRY_AFTER], "3");
        assert!(failure.body.is_none());
        assert!(matches!(failure.body_error, Some(Error::Limit)));
    }
    Ok(())
}

#[tokio::test]
async fn encoded_and_ambiguous_content_types_are_rejected_without_exposing_bodies() -> TestResult {
    for (wire, version) in protocols() {
        let mut encoded = reply(200, snapshot())?;
        encoded
            .headers_mut()
            .insert(header::CONTENT_ENCODING, "gzip".parse()?);
        let mut duplicate = reply(200, snapshot())?;
        duplicate
            .headers_mut()
            .append(header::CONTENT_TYPE, "application/json".parse()?);
        let mut wrong = response(Body::from(TOKEN))?;
        wrong
            .headers_mut()
            .insert(header::CONTENT_TYPE, "text/plain".parse()?);
        let server = server::serve(wire, vec![Some(encoded), Some(duplicate), Some(wrong)]).await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        for _ in 0..3 {
            let error = client
                .freeze_eval_snapshot(&installation, &tenant, &FreezeRequest::default())
                .await
                .err()
                .ok_or("invalid encoding accepted")?;
            let failure = failure(unknown(error)?)?;
            assert_eq!(failure.status, 200);
            assert!(matches!(failure.body_error, Some(Error::Response(_))));
        }
    }
    Ok(())
}

#[tokio::test]
async fn oversized_or_duplicate_subject_collections_are_not_silently_truncated() -> TestResult {
    for (wire, version) in protocols() {
        let subjects: Vec<_> = (0..1025)
            .map(|index| subject_value(&format!("eval-{index}")))
            .collect();
        let server = server::serve(
            wire,
            vec![
                Some(reply(200, json!(subjects))?),
                Some(reply(
                    200,
                    json!([subject_value("eval-1"), subject_value("eval-1")]),
                )?),
            ],
        )
        .await?;
        let client = client(&server, version)?;
        let (installation, tenant) = namespace()?;
        for _ in 0..2 {
            let error = client
                .eval_subjects(&installation, &tenant)
                .await
                .err()
                .ok_or("invalid collection accepted")?;
            assert!(matches!(
                failure(error)?.body_error,
                Some(Error::Response(_))
            ));
        }
    }
    Ok(())
}
