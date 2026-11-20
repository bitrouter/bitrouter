//! Emit the schema for one evaluation exchange document.

use bitrouter_ai::eval::{EvalSnapshot, EvalSubject, EvaluationResult};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let schema = match std::env::args().nth(1).as_deref() {
        Some("subject") => schemars::schema_for!(EvalSubject),
        Some("result") => schemars::schema_for!(EvaluationResult),
        Some("snapshot") => schemars::schema_for!(EvalSnapshot),
        _ => return Err("expected subject, result or snapshot".into()),
    };
    println!("{}", serde_json::to_string_pretty(&schema)?);
    Ok(())
}
