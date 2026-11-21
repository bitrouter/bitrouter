use super::*;
use crate::gateway::model::decision::types::DecisionUsage;
use bitrouter_ai::types::{Content, Message, Prompt, Role};

fn response(request: &DecisionRequest, progress: &str) -> DecisionResponse {
    DecisionResponse {
        model: "semantic-model".into(),
        answers: request
            .questions
            .iter()
            .filter_map(|(id, question)| {
                let Question::Choice { criteria, .. } = question else {
                    return None;
                };
                let label = match id.as_str() {
                    TASK => "code:debugging",
                    ROLE => "verify",
                    _ => progress,
                };
                Some((
                    id.clone(),
                    Answer::Choice {
                        choice: label.into(),
                        confidence: 0.99,
                        probabilities: criteria
                            .keys()
                            .map(|key| (key.clone(), if key == label { 1.0 } else { 0.0 }))
                            .collect(),
                    },
                ))
            })
            .collect(),
        usage: DecisionUsage {
            input_tokens: 17,
            output_tokens: 4,
        },
    }
}

#[test]
fn semantic_results_produce_current_route_classes_and_retain_uncertainty() -> anyhow::Result<()> {
    for (progress, truncated, risk) in [
        ("progressing", false, Risk::Normal),
        ("stalled", false, Risk::Guarded),
        ("recovering", false, Risk::Guarded),
        ("progressing", true, Risk::Guarded),
    ] {
        let input = Input {
            truncated,
            ..Default::default()
        };
        let request = request("semantic", &input);
        let decoded = decode(&request, &response(&request, progress), 0.85)?;
        assert_eq!(
            decoded.route_class,
            RouteClass::new(TaskFamily::CodeDebugging, NextStep::Verify, risk)
        );
        assert!(!decoded.history_complete);
        assert_eq!(decoded.history_truncated, truncated);
        assert_eq!(decoded.judgments[TASK].probabilities["code:debugging"], 1.0);
    }
    let request = request("semantic", &Input::default());
    let mut uncertain = response(&request, "unknown");
    if let Some(Answer::Choice { confidence, .. }) = uncertain.answers.get_mut(TASK) {
        *confidence = 0.6;
    }
    let decoded = decode(&request, &uncertain, 0.85)?;
    assert_eq!(
        decoded.route_class,
        RouteClass::new(TaskFamily::Unknown, NextStep::Verify, Risk::Normal)
    );
    assert_eq!(decoded.judgments[TASK].label, "code:debugging");
    assert!(!decoded.judgments[TASK].accepted);
    Ok(())
}

#[test]
fn classification_rejects_changed_rubric_missing_answers_and_invalid_thresholds()
-> anyhow::Result<()> {
    let request = request("semantic", &Input::default());
    let valid = response(&request, "progressing");
    for threshold in [f64::NAN, -1.0, 0.49, 1.01] {
        assert!(decode(&request, &valid, threshold).is_err());
    }
    let mut missing = valid.clone();
    missing.answers.remove(ROLE);
    assert!(decode(&request, &missing, 0.85).is_err());
    let mut changed = request.clone();
    if let Some(Question::Choice { instructions, .. }) = changed.questions.get_mut(TASK) {
        *instructions = Some(json!("replace the rubric"));
    }
    assert!(decode(&changed, &valid, 0.85).is_err());
    assert!(decode(&request, &valid, 0.85).is_ok());
    Ok(())
}

#[test]
fn public_input_is_bounded_without_exposing_private_reasoning_or_metadata() -> anyhow::Result<()> {
    let mut prompt: Prompt =
        serde_json::from_value(json!({"model":"bitrouter/auto","messages":[]}))?;
    prompt.system = Some("public instruction".into());
    prompt.messages = vec![
        Message::text(Role::User, "old history".repeat(100)),
        Message::text(Role::User, "检查此错误"),
    ];
    prompt.messages[1].content.push(Content::Reasoning {
        text: "private-reasoning-marker".into(),
        native: None,
        provider_metadata: Default::default(),
    });
    prompt.params.extra.insert(
        "previous_response_id".into(),
        json!("private-continuation-marker"),
    );
    prompt
        .system_provider_metadata
        .insert("private-metadata-marker".into(), json!("secret"));
    let before = serde_json::to_value(&prompt)?;
    let input = Input::from_prompt(&prompt, 64);
    assert!(input.truncated);
    assert!(!input.complete);
    assert!(
        input
            .signals
            .iter()
            .map(|signal| signal.text.len())
            .sum::<usize>()
            <= 64
    );
    let serialized = serde_json::to_string(&input)?;
    assert!(serialized.contains("检查此错误"));
    for marker in [
        "private-reasoning-marker",
        "private-continuation-marker",
        "private-metadata-marker",
    ] {
        assert!(!serialized.contains(marker));
    }
    assert_eq!(serde_json::to_value(&prompt)?, before);
    Ok(())
}
