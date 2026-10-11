use std::collections::BTreeMap;

use bitrouter_ai::types::{Message, Prompt, Role};
use bitrouter_sdk::decision_model::types::{Answer, DecisionResponse, DecisionUsage, Question};
use bitrouter_sdk::routing::{assessment, input::Input, signals::NextStepRole};

fn prompt(messages: Vec<Message>) -> Prompt {
    Prompt {
        model: "generation-model".into(),
        system: Some("Keep the task requirements".into()),
        system_provider_metadata: Default::default(),
        messages,
        tools: Vec::new(),
        params: Default::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    }
}

#[test]
fn bounded_projection_keeps_recent_workflow_and_reports_omissions() {
    let input = Input::from_prompt(
        &prompt(vec![
            Message::text(Role::User, "old material ".repeat(4096)),
            Message::text(
                Role::Assistant,
                "The fix is applied; verify the failing test.",
            ),
            Message::text(Role::User, "Run verification now."),
        ]),
        128,
    );
    assert!(input.truncated);
    assert!(!input.complete);
    assert!(
        input
            .signals
            .iter()
            .any(|signal| signal.text == "Run verification now.")
    );
    assert!(
        input
            .signals
            .iter()
            .any(|signal| signal.text.contains("fix is applied"))
    );
    assert!(
        input
            .signals
            .iter()
            .map(|signal| signal.text.len())
            .sum::<usize>()
            <= 128
    );
    assert_eq!(
        input.signals.first().map(|signal| signal.kind.as_str()),
        Some("instruction")
    );
}

#[test]
fn low_confidence_abstains_without_losing_distribution_or_classifier_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let input = Input::from_prompt(
        &prompt(vec![Message::text(Role::User, "Verify the parser fix")]),
        4096,
    );
    let request = assessment::request("decision-alias", &input);
    let mut response = DecisionResponse {
        model: "decision-revision-1".into(),
        usage: DecisionUsage {
            input_tokens: 17,
            output_tokens: 3,
        },
        answers: BTreeMap::new(),
    };
    for (id, question) in &request.questions {
        let Question::Choice { criteria, .. } = question else {
            return Err("unexpected rubric".into());
        };
        let label = match id.as_str() {
            assessment::TASK => "code:debugging",
            assessment::ROLE => "verify",
            assessment::PROGRESS => "recovering",
            _ => return Err("unexpected head".into()),
        };
        response.answers.insert(
            id.clone(),
            Answer::Choice {
                choice: label.into(),
                confidence: if id == assessment::ROLE { 0.8 } else { 0.99 },
                probabilities: criteria
                    .keys()
                    .map(|key| (key.clone(), if key == label { 1.0 } else { 0.0 }))
                    .collect(),
            },
        );
    }
    let strict = assessment::decode(&request, &response, 0.85)?;
    assert_eq!(strict.next_step_role, NextStepRole::Unknown);
    assert_eq!(
        strict.judgments[assessment::ROLE].probabilities["verify"],
        1.0
    );
    assert!(!strict.judgments[assessment::ROLE].accepted);
    let relaxed = assessment::decode(&request, &response, 0.75)?;
    assert_eq!(relaxed.next_step_role, NextStepRole::Verify);
    assert_eq!(strict.contract_digest, relaxed.contract_digest);
    assert_ne!(strict.classifier_digest, relaxed.classifier_digest);
    response.model = "decision-revision-2".into();
    assert_ne!(
        strict.classifier_digest,
        assessment::decode(&request, &response, 0.85)?.classifier_digest
    );
    response.answers.remove(assessment::PROGRESS);
    let invalid = assessment::decode(&request, &response, 0.85)
        .err()
        .ok_or("incomplete response accepted")?;
    assert!(invalid.may_have_run);
    assert_eq!(invalid.usage, Some(response.usage));
    Ok(())
}
