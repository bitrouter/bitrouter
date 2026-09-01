use anyhow::Result;
use bitrouter::output::reports::optimization::{OptimizationControllerReport, TreatmentReport};
use bitrouter::output::{Format, Output};

fn report(action: &'static str, decision: &'static str) -> OptimizationControllerReport {
    OptimizationControllerReport {
        action,
        policy: "auto".into(),
        decision,
        parent_policy_digest: Some(
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".into(),
        ),
        active_policy_digest:
            "sha256:2222222222222222222222222222222222222222222222222222222222222222".into(),
        eval_snapshot_root: Some(
            "sha256:3333333333333333333333333333333333333333333333333333333333333333".into(),
        ),
        observed_subject_digest: Some(
            "sha256:4444444444444444444444444444444444444444444444444444444444444444".into(),
        ),
        treatment: Some(TreatmentReport {
            target_request_key: "agent_route/v1|unknown|implement|normal".into(),
            champion_tier: "strong".into(),
            challenger_tier: "economy".into(),
            challenger_exposure_ppm: 100_000,
            minimum_tasks_per_arm: 3,
            maximum_challenger_tasks: 20,
            minimum_pass_rate_ppm: 900_000,
        }),
        control: None,
        challenger: None,
        cost_delta_micro_usd: None,
        evaluator_config_digest: None,
        published: action == "optimize.run",
        reload_attempted: false,
    }
}

#[test]
fn run_report_is_content_free_and_names_the_controller_transition() -> Result<()> {
    let view = report("optimize.run", "explore");

    let rendered = String::from_utf8(Output::new(Format::Json).render_to_vec(&view))?;

    assert!(rendered.contains("\"action\": \"optimize.run\""));
    assert!(rendered.contains("\"decision\": \"explore\""));
    assert!(rendered.contains("\"challenger_tier\": \"economy\""));
    for forbidden in [
        "prompt",
        "model_output",
        "tool_argument",
        "evaluator_output",
        "credential",
        "task_answer",
        "repository_path",
    ] {
        assert!(!rendered.contains(forbidden), "{forbidden}");
    }
    Ok(())
}

#[test]
fn status_report_is_a_read_only_controller_view() -> Result<()> {
    let view = report("optimize.status", "exploring");

    let rendered = String::from_utf8(Output::new(Format::Human).render_to_vec(&view))?;

    assert!(rendered.contains("optimization is exploring"));
    assert!(rendered.contains("agent_route/v1|unknown|implement|normal"));
    assert!(!rendered.contains("review"));
    assert!(!rendered.contains("bitrouter optimize publish"));
    assert!(!rendered.contains("workflow"));
    Ok(())
}
