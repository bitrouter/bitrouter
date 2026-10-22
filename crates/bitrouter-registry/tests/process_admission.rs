use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

use bitrouter_registry::document::{Document, ReaderPolicy, SPEC_REVISION};
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;
const CHILD: &str = "BITROUTER_REGISTRY_ADMISSION_TEST_CHILD";

#[test]
fn admission_probe_worker() -> TestResult {
    if std::env::var_os(CHILD).is_none() {
        return Ok(());
    }
    for line in std::io::stdin().lock().lines() {
        match Document::parse(line?.as_bytes(), &ReaderPolicy::default()) {
            Ok(_) => println!("registry-probe:accepted"),
            Err(error) => println!("registry-probe:rejected:{}", error.code),
        }
    }
    Ok(())
}

#[test]
fn malicious_numeric_refresh_does_not_kill_the_process_or_prevent_next_admission() -> TestResult {
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", "admission_probe_worker", "--nocapture"])
        .env(CHILD, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let valid = json!({"version":1,"spec_revision":SPEC_REVISION,"as_of":"2026-10-23T00:00:00Z","data":{"models":[],"providers":[]}});
    let mut invalid = valid.clone();
    invalid["future"] = json!("REPLACE_WITH_UNBOUNDED_NUMBER");
    let text = serde_json::to_string(&invalid)?
        .replace("\"REPLACE_WITH_UNBOUNDED_NUMBER\"", "1e999999999");
    let mut input = child.stdin.take().ok_or("child stdin unavailable")?;
    writeln!(input, "{text}")?;
    writeln!(input, "{}", serde_json::to_string(&valid)?)?;
    drop(input);
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "child failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    let events: Vec<_> = stdout
        .lines()
        .filter(|line| line.starts_with("registry-probe:"))
        .collect();
    assert_eq!(
        events,
        [
            "registry-probe:rejected:registry.invalid_data",
            "registry-probe:accepted"
        ]
    );
    Ok(())
}
