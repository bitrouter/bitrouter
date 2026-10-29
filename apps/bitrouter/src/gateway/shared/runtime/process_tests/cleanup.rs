//! Exit-status validation and post-join diagnostic retention for owned fixtures.

use std::collections::BTreeMap;
use std::path::Path;

use super::*;
use crate::gateway::shared::test_database::combine_results;

#[path = "cleanup_tests.rs"]
mod tests;

pub(super) type ExitExpectations = BTreeMap<usize, ExpectedExit>;

#[derive(Clone, Copy)]
pub(super) enum ExpectedExit {
    /// A completed command, such as the separately checked bootstrap child.
    Success,
    /// A serving process with no deliberately unresolved local work.
    CleanServe,
    /// Only the owner whose Unknown execution the public campaign observed.
    /// Durable Unknown can outlive locally drained custody, so a clean exit is
    /// valid too. Exit 1 requires the precise completed-force signature below.
    ObservedUnknown,
}

impl ExpectedExit {
    fn check(self, status: ExitStatus, logs: &str) -> Result<()> {
        match self {
            Self::Success => ensure!(status.success(), "unexpected child exit: {status}"),
            Self::CleanServe | Self::ObservedUnknown => {
                let clean = status.success();
                ensure!(
                    clean || (matches!(self, Self::ObservedUnknown) && status.code() == Some(1)),
                    "unexpected serving child exit: {status}"
                );
                let logs = without_color(logs);
                let mut reports = logs
                    .lines()
                    .filter_map(|line| line.split_once("shared gateway shutdown completed"));
                let (_, fields) = reports
                    .next()
                    .context("missing completed shutdown report")?;
                ensure!(
                    reports.next().is_none(),
                    "multiple completed shutdown reports"
                );
                let mut actual = BTreeMap::new();
                for field in fields.split_whitespace() {
                    let (name, value) = field
                        .split_once('=')
                        .context("invalid shutdown report field")?;
                    ensure!(
                        actual.insert(name, value).is_none(),
                        "duplicate shutdown report field"
                    );
                }
                let expected = BTreeMap::from([
                    ("forced", if clean { "false" } else { "true" }),
                    ("withdrawal", "Confirmed"),
                    ("quiescent", "true"),
                    ("durable_resolved", if clean { "true" } else { "false" }),
                    ("clean", if clean { "true" } else { "false" }),
                ]);
                ensure!(
                    actual == expected,
                    "unexpected completed shutdown report: {fields}"
                );
                if !clean {
                    // runtime::serve_with_pools checks its primary result BEFORE
                    // the clean-report assertion. A worker/startup/DB failure
                    // has a different envelope and cannot use this exception.
                    // main::async_main emits pretty JSON and exits exactly 1;
                    // main-thread panic is 101, and signal exits have no code.
                    let envelope_start = logs
                        .lines()
                        .scan(0, |offset, line| {
                            let start = *offset;
                            *offset += line.len() + 1;
                            Some((start, line))
                        })
                        .find_map(|(start, line)| (line == "{").then_some(start))
                        .context("missing shutdown error envelope")?;
                    let envelope: Value = serde_json::from_str(&logs[envelope_start..])
                        .context("invalid or additional shutdown error output")?;
                    ensure!(
                        envelope
                            == json!({"error": {
                                "kind": "internal",
                                "message": "shared gateway stopped with interrupted or unresolved work"
                            }}),
                        "unexpected shutdown error envelope"
                    );
                }
            }
        }
        Ok(())
    }
}

// The standard tracing formatter can emit ANSI SGR even into captured files.
// Strip only those color/style sequences; preserve all diagnostic text.
fn without_color(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            let mut probe = chars.clone();
            probe.next();
            while probe
                .peek()
                .is_some_and(|ch| ch.is_ascii_digit() || *ch == ';')
            {
                probe.next();
            }
            if probe.next() == Some('m') {
                chars = probe;
                continue;
            }
        }
        output.push(ch);
    }
    output
}

pub(super) async fn finish_children(
    children: &mut [Running],
    primary: Result<()>,
    expected: &ExitExpectations,
    prefix: &Path,
) -> Result<()> {
    // join_all never short-circuits: all retained children reach stop/wait before
    // status interpretation, copying diagnostics, or returning any failure.
    let stopped = futures::future::join_all(children.iter_mut().map(Running::stop)).await;
    let mut cleanup = Vec::new();
    for (index, (child, result)) in children.iter().zip(stopped).enumerate() {
        cleanup.push(
            result
                .and_then(|status| {
                    let expectation =
                        expected
                            .get(&index)
                            .copied()
                            .unwrap_or(if child.address.port() == 0 {
                                ExpectedExit::Success
                            } else {
                                ExpectedExit::CleanServe
                            });
                    let logs =
                        std::fs::read_to_string(&child.log).context("reading final child log")?;
                    expectation.check(status, &logs)
                })
                .with_context(|| format!("joining fixture gateway {index}")),
        );
    }
    for index in expected.keys() {
        if *index >= children.len() {
            cleanup.push(Err(anyhow::anyhow!(
                "exit expectation names missing child {index}"
            )));
        }
    }
    let result = combine_results(primary, cleanup);
    retain_failed_logs(
        children.iter().map(|child| child.log.as_path()),
        result,
        prefix,
    )
}

pub(super) fn retain_failed_logs<'a>(
    logs: impl Iterator<Item = &'a Path>,
    result: Result<()>,
    prefix: &Path,
) -> Result<()> {
    if result.is_ok() {
        return result;
    }
    let copies = logs
        .enumerate()
        .map(|(index, log)| {
            let path = prefix.with_extension(format!("{index}.log"));
            std::fs::copy(log, &path)
                .map(|_| ())
                .with_context(|| format!("retaining failed child log {}", path.display()))
        })
        .collect();
    combine_results(result, copies)
        .with_context(|| format!("final fixture child logs: {}.*.log", prefix.display()))
}
