use super::*;
use axum::body::Bytes;
use bitrouter_ai::tenancy::usage_evidence_v2::EvidenceBudgetV2;
use bitrouter_sdk::error::Error;

fn padded_response(value: &Value, size: usize, chunked: bool) -> TestResult<Response> {
    let mut bytes = serde_json::to_vec(value)?;
    assert!(bytes.len() < size);
    bytes.resize(size, b' ');
    let body = if chunked {
        let chunks = bytes
            .chunks(4096)
            .map(Bytes::copy_from_slice)
            .collect::<Vec<_>>();
        HttpBody::from_stream(futures_util::stream::iter(
            chunks.into_iter().map(Ok::<_, std::io::Error>),
        ))
    } else {
        HttpBody::from(bytes)
    };
    Ok(Response::builder()
        .header("content-type", "application/json")
        .body(body)?)
}

#[tokio::test]
async fn skipped_candidates_keep_accepted_budget_and_original_nonempty_observations() -> TestResult
{
    let mut receipt = receipt(9)?;
    receipt.attempts.drain(..8);
    receipt.validate()?;
    assert_eq!(receipt.attempts[0].candidate_index, 8);
    let mut value = serde_json::to_value(evidence(&receipt)?)?;
    value["frames"] = json!([{"sequence":0,"attempt_id":"attempt-8","protocol":"chat_completions",
        "source":"http_error","semantics":"snapshot","quantities":[
            {"dimension":"input_tokens","value":0},{"dimension":"output_tokens","value":null}],
        "unsupported":false,"invalid":true}]);
    serde_json::from_value::<ExecutionUsageEvidenceV2>(value.clone())?
        .validate_against(&receipt)?;
    let budget =
        EvidenceBudgetV2::for_attempts(receipt.accepted_selection.candidate_indices.len())?;
    assert!(budget.max_bytes > EvidenceBudgetV2::for_attempts(receipt.attempts.len())?.max_bytes);

    for (version, wire) in versions() {
        for chunked in [false, true] {
            let server = server::serve(
                wire,
                vec![
                    Some(padded_response(&value, budget.max_bytes, chunked)?),
                    Some(padded_response(&value, budget.max_bytes + 1, chunked)?),
                ],
            )
            .await?;
            let client = control_client(&server, version)?;
            let observed = client.usage_evidence_v2(&receipt).await?;
            assert_eq!(observed.accepted_selection.candidate_indices.len(), 9);
            assert_eq!(observed.attempts.len(), 1);
            assert_eq!(observed.attempts[0].attempt_id.as_str(), "attempt-8");
            assert_eq!(serde_json::to_value(observed)?, value);
            let Err(Error::Tenancy(failure)) = client.usage_evidence_v2(&receipt).await else {
                return Err("oversized evidence accepted".into());
            };
            assert!(matches!(failure.body_error, Some(Error::Limit)));
            assert_eq!(server.records.lock().await.len(), 2);
        }
    }
    Ok(())
}
