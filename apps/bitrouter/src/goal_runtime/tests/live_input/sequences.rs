//! Continuous actual-model input trajectories; no harness dispatch/adoption claim.
use super::*;

pub(super) async fn evaluate_continuous() -> Result<()> {
    let definitions = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/evidence/bro-goal-workflow/input-eval-sequences.jsonl"
    ))
    .lines()
    .map(|line| serde_json::from_str::<serde_json::Value>(line).map_err(anyhow::Error::from))
    .collect::<Result<Vec<_>>>()?;
    let selected = definitions
        .into_iter()
        .filter(|definition| {
            matches!(
                definition["id"].as_str(),
                Some("s01" | "s02" | "s03" | "s06" | "s07" | "s08")
            )
        })
        .collect::<Vec<_>>();
    anyhow::ensure!(selected.len() == 6, "continuous input definitions missing");
    evaluate_inputs(selected).await
}

pub(super) struct FaultBackend {
    pub server: MockServer,
    pub reasoning_timeout: bool,
}

/// Controlled transport failures, never advertised as actual provider outputs.
pub(super) async fn inject_backend(definition: &serde_json::Value) -> Result<Option<FaultBackend>> {
    let Some(fault) = definition.get("backend_fault") else {
        return Ok(None);
    };
    let server = MockServer::start().await;
    let response = match fault["typed"].as_str() {
        Some("timeout") => wiremock::ResponseTemplate::new(200).set_delay(Duration::from_secs(2)),
        Some("unavailable") => wiremock::ResponseTemplate::new(503),
        Some("contradictory_distribution") => {
            Mock::given(method("POST"))
                .and(path("/v1/systemone"))
                .respond_with(contradictory)
                .mount(&server)
                .await;
            return Ok(Some(FaultBackend {
                server,
                reasoning_timeout: false,
            }));
        }
        _ => anyhow::bail!("unknown typed fault fixture"),
    };
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(response)
        .mount(&server)
        .await;
    let reasoning_timeout = fault["reasoning"] == "timeout";
    if reasoning_timeout {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
            .mount(&server)
            .await;
    }
    Ok(Some(FaultBackend {
        server,
        reasoning_timeout,
    }))
}

fn contradictory(request: &wiremock::Request) -> wiremock::ResponseTemplate {
    let Ok(body) = serde_json::from_slice::<serde_json::Value>(&request.body) else {
        return wiremock::ResponseTemplate::new(400);
    };
    let Some(questions) = body["questions"].as_object() else {
        return wiremock::ResponseTemplate::new(400);
    };
    let mut answers = serde_json::Map::new();
    for (id, question) in questions {
        let Some(criteria) = question["criteria"].as_object() else {
            return wiremock::ResponseTemplate::new(400);
        };
        let Some(first) = criteria.keys().next() else {
            return wiremock::ResponseTemplate::new(400);
        };
        let choice = if id == "control" { "none" } else { first };
        let mass = if id == "control" { "stop" } else { first };
        let probabilities = criteria
            .keys()
            .map(|label| {
                (
                    label.clone(),
                    serde_json::json!(if label == mass { 1.0 } else { 0.0 }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        answers.insert(
            id.clone(),
            serde_json::json!({"type":"choice", "choice":choice,
            "confidence":1.0,"probabilities":probabilities}),
        );
    }
    wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "model":body["model"],"answers":answers,"usage":{"input_tokens":10,"output_tokens":3}
    }))
}

pub(super) async fn fault_violations(
    definition: &serde_json::Value,
    typed: bool,
    classification: Option<&serde_json::Value>,
    before: &GoalSnapshot,
    after: &GoalSnapshot,
    reports: &[serde_json::Value],
    server: &MockServer,
) -> Result<Vec<String>> {
    let requests = server
        .received_requests()
        .await
        .context("fault requests unavailable")?;
    let mut violations = Vec::new();
    let typed_requests = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/systemone")
        .collect::<Vec<_>>();
    if typed_requests.len() != usize::from(typed) {
        violations.push("injected typed fault was skipped or retried".into());
    }
    if typed {
        let expected = match definition["backend_fault"]["typed"].as_str() {
            Some("timeout") => "timeout",
            Some("contradictory_distribution") => "invalid_response",
            Some("unavailable") => "http",
            _ => anyhow::bail!("unknown expected fault"),
        };
        let error = classification.map(|value| &value["outcome"]["Err"]);
        if error.is_none_or(|error| error["kind"] != expected || error["may_have_run"] != true) {
            violations.push("typed failure or uncertain dispatch was not retained".into());
        }
        if expected == "invalid_response"
            && error.is_none_or(|error| error["usage"]["input_tokens"] != 10)
        {
            violations.push("contradictory typed response lost reported usage".into());
        }
        if let Some(request) = typed_requests.first() {
            let body: serde_json::Value = serde_json::from_slice(&request.body)?;
            if body["state"]["input"] != definition["input"] {
                violations.push("fault upgrade changed the original user input".into());
            }
        }
    }
    if reports.iter().all(|record| record.get("plan").is_none()) {
        violations.push("fault did not enter a durably admitted reasoning attempt".into());
    }
    if definition["backend_fault"]["reasoning"] == "timeout" {
        if requests
            .iter()
            .all(|request| request.url.path() != "/v1/chat/completions")
            || reports
                .iter()
                .all(|record| record["observation"]["error"].is_null())
        {
            violations
                .push("reasoning timeout has no actual HTTP attempt and failure evidence".into());
        }
        let result = after.inputs.latest.as_ref();
        if result.is_none_or(|result| {
            result.clarification.is_none()
                || result.clarification_kind
                    != bitrouter_orchestrator::goal::input::ClarificationKind::Unknown
                || result.requirements_saved
                || result.control_saved.is_some()
        }) || !after
            .inputs
            .unresolved_inputs()
            .iter()
            .any(|input| input.input_id == "u1")
        {
            violations.push(
                "understanding failure invented effects or erased the earlier request".into(),
            );
        }
    }
    if (definition["backend_fault"]["typed"] == "timeout" && typed
        || definition["backend_fault"]["reasoning"] == "timeout")
        && (after.accounting.unpriced_outcomes <= before.accounting.unpriced_outcomes
            || after.budget.held.is_empty())
    {
        violations
            .push("unknown billed outcome lost its accounting or conservative reservation".into());
    }
    Ok(violations)
}
