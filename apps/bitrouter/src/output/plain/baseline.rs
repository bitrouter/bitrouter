//! Replay retained output captured from the actual pre-refactor executable code.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use agent_client_protocol_schema::v1::SessionUpdate;
use anyhow::{Context as _, Result};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::{PIPED, Transcript, write};

#[derive(Deserialize)]
struct Manifest {
    baseline_commit: String,
    piped_extent: Extent,
    captures: Vec<Capture>,
}

#[derive(Deserialize)]
struct Extent {
    width: u16,
    height: u16,
}

#[derive(Deserialize)]
struct Capture {
    id: String,
    driver: String,
    input: String,
    expected: String,
    #[serde(default)]
    checkpoints: Vec<Checkpoint>,
    #[serde(default)]
    captures: Vec<Checkpoint>,
}

#[derive(Deserialize)]
struct Checkpoint {
    id: String,
    step_index: Option<usize>,
    byte_start: usize,
    byte_end: usize,
    rows: usize,
    sha256: String,
}

#[derive(Deserialize)]
struct Sequence {
    id: String,
    steps: Vec<Step>,
}

// ACP notification payloads are captured through the baseline published schema.
// https://agentclientprotocol.com/protocol/session-update
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Step {
    Update { update: Box<SessionUpdate> },
    Checkpoint { id: String },
}

fn fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/transcript/cases")
}

fn capture(id: &str) -> Result<Capture> {
    let manifest: Manifest =
        serde_json::from_slice(&std::fs::read(fixture_root().join("captures.json"))?)?;
    assert_eq!(
        manifest.baseline_commit,
        "9956a427bd5d4316b3bf2359f88e293b158781d5"
    );
    assert_eq!(
        (manifest.piped_extent.width, manifest.piped_extent.height),
        (PIPED.width, PIPED.height)
    );
    let mut matches = manifest
        .captures
        .into_iter()
        .filter(|capture| capture.id == id);
    let capture = matches.next().context("missing baseline capture")?;
    assert!(matches.next().is_none(), "duplicate capture ID: {id}");
    Ok(capture)
}

fn compare(checkpoint: &Checkpoint, expected: &[u8], actual: &[u8], rows: usize) -> Result<()> {
    let recorded = expected
        .get(checkpoint.byte_start..checkpoint.byte_end)
        .context("invalid recorded byte range")?;
    assert_eq!(
        hex::encode(Sha256::digest(recorded)),
        checkpoint.sha256,
        "fixture checksum: {}",
        checkpoint.id
    );
    assert_eq!(rows, checkpoint.rows, "row count: {}", checkpoint.id);
    assert_eq!(
        std::str::from_utf8(actual)?,
        std::str::from_utf8(recorded)?,
        "checkpoint {}",
        checkpoint.id
    );
    Ok(())
}

fn replay(id: &str) -> Result<()> {
    let capture = capture(id)?;
    assert_eq!(capture.driver, "plain_transcript");
    let sequence: Sequence =
        serde_json::from_slice(&std::fs::read(fixture_root().join(&capture.input))?)?;
    assert_eq!(sequence.id, id);
    let expected = std::fs::read(fixture_root().join(&capture.expected))?;
    let mut checkpoints = capture.checkpoints.iter();
    let mut transcript = Transcript::default();
    let mut complete = Vec::new();
    for (index, step) in sequence.steps.into_iter().enumerate() {
        match step {
            Step::Update { update } => transcript.apply(*update),
            Step::Checkpoint { id } => {
                let checkpoint = checkpoints.next().context("missing checkpoint evidence")?;
                assert_eq!(checkpoint.id, id);
                assert_eq!(checkpoint.step_index, Some(index));
                assert_eq!(
                    checkpoint.byte_start,
                    complete.len(),
                    "noncontiguous capture"
                );
                let rows = transcript.unwritten();
                let mut actual = Vec::new();
                write(&mut actual, &rows)?;
                compare(checkpoint, &expected, &actual, rows.len())?;
                complete.extend(actual);
            }
        }
    }
    assert!(
        checkpoints.next().is_none(),
        "unexercised checkpoint evidence"
    );
    assert_eq!(complete, expected, "complete transcript {id}");
    Ok(())
}

#[test]
fn baseline_markdown_unicode_and_settled_turns() -> Result<()> {
    replay("markdown-unicode")
}

// The recorded diff paths are POSIX absolute paths. The ordinary diff tests
// remain portable and exercise native path rendering on Windows too.
#[cfg(unix)]
#[test]
fn baseline_tool_patches_diffs_and_reasoning() -> Result<()> {
    replay("tools-diffs-reasoning")
}

#[test]
fn baseline_cost_updates_do_not_add_a_plain_footer() -> Result<()> {
    replay("cost-omission")
}

#[derive(Deserialize)]
struct LogTail {
    id: String,
    path: PathBuf,
    log: String,
    max_lines: usize,
}

#[test]
fn baseline_failure_tails_keep_text_and_omission_counts() -> Result<()> {
    let capture = capture("failure-tails")?;
    assert_eq!(capture.driver, "log_tail_then_plain_write");
    let tails: Vec<LogTail> =
        serde_json::from_slice(&std::fs::read(fixture_root().join(&capture.input))?)?;
    let expected = std::fs::read(fixture_root().join(&capture.expected))?;
    assert_eq!(tails.len(), capture.captures.len());
    let mut complete = Vec::new();
    for (tail, checkpoint) in tails.iter().zip(&capture.captures) {
        assert_eq!(tail.id, checkpoint.id);
        assert_eq!(checkpoint.byte_start, complete.len());
        let lines = crate::presentation::log_tail::render(&tail.path, &tail.log, tail.max_lines);
        let mut actual = Vec::new();
        // This is the current piped-chat serialization path, without styling.
        for line in &lines {
            writeln!(&mut actual, "{line}")?;
        }
        compare(checkpoint, &expected, &actual, lines.len())?;
        complete.extend(actual);
    }
    assert_eq!(complete, expected);
    Ok(())
}

#[derive(Deserialize)]
struct ConstructorCase {
    id: String,
    constructor: String,
    text: String,
}

#[test]
fn baseline_row_and_run_constructors_retain_distinct_separator_handling() -> Result<()> {
    use crate::presentation::transcript::text::{Format, Row, Run, Tone};

    let capture = capture("line-constructors")?;
    assert_eq!(capture.driver, "line_constructors_then_plain_write");
    let cases: Vec<ConstructorCase> =
        serde_json::from_slice(&std::fs::read(fixture_root().join(&capture.input))?)?;
    let expected = std::fs::read(fixture_root().join(&capture.expected))?;
    assert_eq!(cases.len(), capture.captures.len());
    let mut complete = Vec::new();
    for (case, checkpoint) in cases.iter().zip(&capture.captures) {
        assert_eq!(case.id, checkpoint.id);
        assert_eq!(checkpoint.byte_start, complete.len());
        let row = match case.constructor.as_str() {
            "line_raw" | "line_from_string" => Row::from(case.text.clone()),
            "line_from_str" => Row::from(case.text.as_str()),
            "span_raw" => Row::from(Run::raw(&case.text)),
            "span_styled" => Row::from(Run::annotated(
                &case.text,
                Format::default().tone(Tone::Pending).bold(),
            )),
            other => anyhow::bail!("unknown baseline constructor {other}"),
        };
        let mut actual = Vec::new();
        write(&mut actual, &[row])?;
        compare(checkpoint, &expected, &actual, 1)?;
        complete.extend(actual);
    }
    assert_eq!(complete, expected);
    Ok(())
}
