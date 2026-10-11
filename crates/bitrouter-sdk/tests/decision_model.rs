//! TypeSafe wire and failure contract; no live provider calls or quality claims.

use std::time::Duration;

use bitrouter_sdk::decision_model::DecisionExecutor;
use bitrouter_sdk::decision_model::types::{DecisionFailure, DecisionRequest, DecisionResponse};
use bitrouter_sdk::decision_model::typesafe::TypeSafeExecutor;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn request() -> Result<DecisionRequest, serde_json::Error> {
    serde_json::from_value(json!({
        "model": "jev-latest",
        "state": {"task": "Repair auth", "block": "Login rejects expired sessions"},
        "questions": {
            "relevance": {"type": "noul", "instructions": "Does block help task?"},
            "representation": {"type": "choice", "instructions": "Select a representation for block in task",
                "criteria": {"full": "Exact details matter", "hide": "Unrelated to task"}},
            "sufficiency": {"type": "score", "instructions": "Rate block's coverage of task",
                "criteria": ["Insufficient", "Partial", "Complete"]}
        }
    }))
}

fn response() -> Value {
    json!({
        "model": "jev-1.13",
        "answers": {
            "relevance": {"type": "noul", "noul": 0.95},
            "representation": {"type": "choice", "choice": "full", "confidence": 0.9,
                "probabilities": {"full": 0.95, "hide": 0.05}},
            "sufficiency": {"type": "score", "score": 1.7, "confidence": 0.8,
                "legend": {"0": "Insufficient", "1": "Partial", "2": "Complete"},
                "probabilities": {"0": 0.1, "1": 0.1, "2": 0.8}}
        },
        "usage": {"input_tokens": 120, "output_tokens": 12}
    })
}

fn executor(
    server: &MockServer,
    timeout: Duration,
    bytes: usize,
) -> Result<TypeSafeExecutor, Box<dyn std::error::Error>> {
    Ok(TypeSafeExecutor::new(
        &server.uri(),
        "fixture-token",
        timeout,
        bytes,
    )?)
}

#[tokio::test]
async fn mixed_primitives_use_systemone_and_accept_resolved_model()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    let request = request()?;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer fixture-token"))
        .and(body_json(serde_json::to_value(&request)?))
        .respond_with(ResponseTemplate::new(200).set_body_json(response()))
        .expect(1)
        .mount(&server)
        .await;
    let result = executor(&server, Duration::from_secs(10), 8192)?
        .execute(&request, &CancellationToken::new())
        .await?;
    assert_eq!(result.model, "jev-1.13");
    assert_eq!(result.usage.input_tokens, 120);
    Ok(())
}

#[test]
fn rejects_malformed_answers_and_retains_usage() -> Result<(), Box<dyn std::error::Error>> {
    let request = request()?;
    let mut invalid = Vec::new();
    let mut value = response();
    value["answers"]["relevance"]["noul"] = json!(1.01);
    invalid.push(value);
    let mut value = response();
    value["answers"]["representation"]["choice"] = json!("hide");
    invalid.push(value);
    let mut value = response();
    value["answers"]["representation"]["probabilities"]["injected"] = json!(0.0);
    invalid.push(value);
    let mut value = response();
    value["answers"]["sufficiency"]["score"] = json!(2.0);
    invalid.push(value);
    let mut value = response();
    value["answers"]["sufficiency"]["legend"]["1"] = json!("Changed rubric");
    invalid.push(value);
    let mut value = response();
    value["answers"]["extra"] = json!({"type": "noul", "noul": 1.0});
    invalid.push(value);
    let mut value = response();
    value["answers"]["relevance"] = json!({"type": "choice", "choice": "yes", "confidence": 1.0,
        "probabilities": {"yes": 1.0}});
    invalid.push(value);
    for value in invalid {
        let decoded: DecisionResponse = serde_json::from_value(value)?;
        let Err(error) = decoded.validate(&request) else {
            return Err("invalid response was accepted".into());
        };
        assert_eq!(error.kind, DecisionFailure::InvalidResponse);
        assert_eq!(error.usage.map(|usage| usage.input_tokens), Some(120));
        assert!(error.may_have_run);
    }
    Ok(())
}

#[tokio::test]
async fn malformed_response_and_http_failure_do_not_retry() -> Result<(), Box<dyn std::error::Error>>
{
    for status in [200, 429] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(status).set_body_json(json!({
                "model": "jev-latest", "answers": {"malformed": {"secret": "never log this"}},
                "usage": {"input_tokens": 17, "output_tokens": 3}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let result = executor(&server, Duration::from_secs(10), 8192)?
            .execute(&request()?, &CancellationToken::new())
            .await;
        let Err(error) = result else {
            return Err("malformed response was accepted".into());
        };
        assert_eq!(error.usage.map(|usage| usage.input_tokens), Some(17));
        assert!(!error.to_string().contains("never log"));
        assert_eq!(
            error.kind,
            if status == 200 {
                DecisionFailure::InvalidResponse
            } else {
                DecisionFailure::Http
            }
        );
    }
    Ok(())
}

#[tokio::test]
async fn cancellation_timeout_and_response_limits_preserve_unknown_usage()
-> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .set_body_json(response()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let client = executor(&server, Duration::from_millis(100), 8192)?;
    let token = CancellationToken::new();
    token.cancel();
    let Err(cancelled) = client.execute(&request()?, &token).await else {
        return Err("cancelled request succeeded".into());
    };
    assert_eq!(cancelled.kind, DecisionFailure::Cancelled);
    assert!(!cancelled.may_have_run);
    let Err(timeout) = client.execute(&request()?, &CancellationToken::new()).await else {
        return Err("slow request succeeded".into());
    };
    assert_eq!(timeout.kind, DecisionFailure::Timeout);
    assert!(timeout.may_have_run);
    assert!(timeout.usage.is_none());

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response()))
        .expect(1)
        .mount(&server)
        .await;
    let Err(large) = executor(&server, Duration::from_secs(10), 16)?
        .execute(&request()?, &CancellationToken::new())
        .await
    else {
        return Err("oversized response accepted".into());
    };
    assert_eq!(large.kind, DecisionFailure::ResponseTooLarge);
    assert!(large.may_have_run);
    Ok(())
}

#[tokio::test]
async fn invalid_request_never_dispatches() -> Result<(), Box<dyn std::error::Error>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let mut request = request()?;
    request.questions.clear();
    let Err(error) = executor(&server, Duration::from_secs(10), 8192)?
        .execute(&request, &CancellationToken::new())
        .await
    else {
        return Err("empty questions accepted".into());
    };
    assert_eq!(error.kind, DecisionFailure::InvalidRequest);
    assert!(!error.may_have_run);
    Ok(())
}
