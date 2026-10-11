//! Deterministic semantic provider fixtures. Labels are test inputs, never a
//! classifier quality claim or an alternative production routing algorithm.
use bitrouter::policy_lock::{
    CertificateSource, PolicyCertificate, PolicyLock, PromotionVerdict, RouteOwner,
};
use bitrouter_sdk::config::Config;
use bitrouter_sdk::config::decision::DecisionModelConfig;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

pub fn certify_routes(lock: &mut PolicyLock) {
    for (name, policy) in &mut lock.policies {
        if !policy.routes.is_empty() {
            policy.predictor =
                Some(bitrouter::workflow_state::predictive::compiled_predictor_contract());
        }
        for (key, tier) in &policy.routes {
            lock.certificates.entry(name.clone()).or_default().insert(
                key.clone(),
                PolicyCertificate {
                    classifier_digest: None,
                    owner: RouteOwner::Operator,
                    selected_tier: tier.clone(),
                    baseline_tier: policy.default_tier.clone(),
                    source: CertificateSource::Operator,
                    eligible_episodes: 0,
                    independent_tasks: 0,
                    quality: None,
                    economics: None,
                    latency: None,
                    critical_violations: 0,
                    verdict: PromotionVerdict::Retain,
                    evaluator_config_digest: None,
                    compiler_config_digest: format!("sha256:{}", "0".repeat(64)),
                    evidence_digest: format!("sha256:{}", "0".repeat(64)),
                },
            );
        }
    }
}

pub async fn attach(
    config: &mut Config,
    labels: fn(&Value) -> (&str, &str, &str),
) -> anyhow::Result<MockServer> {
    let backend = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/systemone"))
        .respond_with(move |request: &Request| {
            let Ok(body) = serde_json::from_slice::<Value>(&request.body) else { return ResponseTemplate::new(400); };
            let (task, role, progress) = labels(&body["state"]["routing"]);
            let mut answers = serde_json::Map::new();
            if let Some(questions) = body["questions"].as_object() {
                for (id, question) in questions {
                    let label = match id.as_str() {
                        "routing_task_family" => task,
                        "routing_next_role" => role,
                        "routing_progress" => progress,
                        _ => "full",
                    };
                    let Some(criteria) = question["criteria"].as_object() else { return ResponseTemplate::new(400); };
                    let probabilities: serde_json::Map<_, _> = criteria.keys().map(|key| (key.clone(), json!(if key == label { 1.0 } else { 0.0 }))).collect();
                    answers.insert(id.clone(), json!({"type":"choice", "choice":label, "confidence":1.0, "probabilities":probabilities}));
                }
            }
            ResponseTemplate::new(200).set_body_json(json!({"model":"fixture-v1", "answers": answers, "usage":{"input_tokens":10,"output_tokens":3}}))
        }).mount(&backend).await;
    // The local fixture accepts the host account name as an opaque credential.
    let key_env = if std::env::var("USER").is_ok() {
        "USER"
    } else {
        "USERNAME"
    };
    anyhow::ensure!(
        std::env::var(key_env).is_ok(),
        "local fixture needs an account name"
    );
    config.decision_model = Some(serde_json::from_value::<DecisionModelConfig>(
        json!({"model":"fixture", "base_url":backend.uri(), "api_key_env":key_env}),
    )?);
    Ok(backend)
}
