//! Consistency checks for the committed `benchmarks/` evidence.
//!
//! Each run under `benchmarks/runs/<date>-<name>/` publishes a machine-readable
//! `results.json`, a human-readable `report.md` with a results table, and the
//! derived evidence under `data/`. These three must agree. This checker guards
//! that they do:
//!
//! 1. `results.json` is internally consistent (every derived figure recomputes);
//! 2. `results.json` matches the numbers in the `report.md` table; and
//! 3. `results.json` matches the committed `data/<group>/benchmark-outcomes.jsonl`.
//!
//! It never writes — it only reads and, on any mismatch, fails with a message
//! naming the run, group, and field. It is generic over every run directory, so
//! future runs are covered without changes here.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Results {
    run_id: String,
    comparable_task_count: u32,
    groups: Vec<Group>,
    lifecycle: Lifecycle,
}

#[derive(Debug, Deserialize)]
struct Group {
    group: String,
    passed: u32,
    total: u32,
    score_pct: f64,
    total_requests: u32,
    strong_calls: u32,
    weak_calls: u32,
    weak_share_pct: f64,
    cost_usd: f64,
    cost_per_success_usd: f64,
    cost_vs_control_pct: f64,
    score_vs_control_pp: f64,
}

#[derive(Debug, Deserialize)]
struct Lifecycle {
    policy_total_cost_usd: f64,
    policy_aggregate_successes: u32,
}

/// Check every run under `benchmarks/runs/`. A missing `benchmarks/` directory
/// is not an error — the check is a no-op until the first run is committed.
pub fn check(root: &Path) -> Result<()> {
    let runs_dir = root.join("benchmarks/runs");
    if !runs_dir.is_dir() {
        return Ok(());
    }

    let mut dirs: Vec<_> = fs::read_dir(&runs_dir)
        .with_context(|| format!("reading {}", runs_dir.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("listing {}", runs_dir.display()))?
        .into_iter()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();

    for dir in &dirs {
        check_run(dir).with_context(|| format!("benchmark run {}", dir.display()))?;
    }
    println!("benchmarks: {} run(s) consistent", dirs.len());
    Ok(())
}

fn check_run(dir: &Path) -> Result<()> {
    let results_path = dir.join("results.json");
    let results_raw = fs::read_to_string(&results_path)
        .with_context(|| format!("reading {}", results_path.display()))?;
    let results: Results = serde_json::from_str(&results_raw)
        .with_context(|| format!("parsing {}", results_path.display()))?;

    validate_results(&results).context("results.json is internally inconsistent")?;

    let report_path = dir.join("report.md");
    let report = fs::read_to_string(&report_path)
        .with_context(|| format!("reading {}", report_path.display()))?;
    check_report_table(&report, &results).context("results.json disagrees with report.md table")?;

    if !report.contains(&results.run_id) {
        bail!("run_id `{}` does not appear in report.md", results.run_id);
    }

    // If the derived evidence is committed, the outcome rows must reproduce the
    // published per-group pass counts exactly.
    let data_dir = dir.join("data");
    if data_dir.is_dir() {
        for g in &results.groups {
            let path = data_dir.join(&g.group).join("benchmark-outcomes.jsonl");
            let raw =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let (total, passed) = count_outcomes(&raw)
                .with_context(|| format!("counting outcomes in {}", path.display()))?;
            if total != g.total || passed != g.passed {
                bail!(
                    "data/{}/benchmark-outcomes.jsonl has {passed}/{total}, results.json says {}/{}",
                    g.group,
                    g.passed,
                    g.total
                );
            }
        }
    }
    Ok(())
}

/// Recompute every derived figure in `results.json` from its primitives.
fn validate_results(r: &Results) -> Result<()> {
    let control = r
        .groups
        .iter()
        .find(|g| g.group == "control")
        .context("no `control` group in results.json")?;

    for g in &r.groups {
        if g.total != r.comparable_task_count {
            bail!(
                "group {}: total {} != comparable_task_count {}",
                g.group,
                g.total,
                r.comparable_task_count
            );
        }
        if g.strong_calls + g.weak_calls != g.total_requests {
            bail!(
                "group {}: strong_calls + weak_calls ({}) != total_requests {}",
                g.group,
                g.strong_calls + g.weak_calls,
                g.total_requests
            );
        }
        close(
            &g.group,
            "score_pct",
            pct(g.passed, g.total),
            g.score_pct,
            0.01,
        )?;
        close(
            &g.group,
            "score_vs_control_pp",
            (f64::from(g.passed) - f64::from(control.passed)) / f64::from(g.total) * 100.0,
            g.score_vs_control_pp,
            0.01,
        )?;
        if g.total_requests > 0 {
            close(
                &g.group,
                "weak_share_pct",
                pct(g.weak_calls, g.total_requests),
                g.weak_share_pct,
                0.01,
            )?;
        }
        if g.passed > 0 {
            close(
                &g.group,
                "cost_per_success_usd",
                g.cost_usd / f64::from(g.passed),
                g.cost_per_success_usd,
                0.011,
            )?;
        }
        close(
            &g.group,
            "cost_vs_control_pct",
            (g.cost_usd - control.cost_usd) / control.cost_usd * 100.0,
            g.cost_vs_control_pct,
            0.01,
        )?;
    }

    let policy_cost: f64 = r
        .groups
        .iter()
        .filter(|g| g.group != "control")
        .map(|g| g.cost_usd)
        .sum();
    close(
        "lifecycle",
        "policy_total_cost_usd",
        policy_cost,
        r.lifecycle.policy_total_cost_usd,
        0.01,
    )?;
    let policy_successes: u32 = r
        .groups
        .iter()
        .filter(|g| g.group != "control")
        .map(|g| g.passed)
        .sum();
    if policy_successes != r.lifecycle.policy_aggregate_successes {
        bail!(
            "lifecycle policy_aggregate_successes {} != sum of policy passes {policy_successes}",
            r.lifecycle.policy_aggregate_successes
        );
    }
    Ok(())
}

/// Match each group's `results.json` row against the `report.md` results table.
fn check_report_table(report: &str, results: &Results) -> Result<()> {
    for g in &results.groups {
        let row = report
            .lines()
            .find(|line| {
                let cells: Vec<&str> = line.split('|').map(str::trim).collect();
                cells.len() > 2 && cells[1] == g.group
            })
            .with_context(|| format!("no report table row for group {}", g.group))?;
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        // Columns: | Group | Passed | Score | Total requests | GPT-5.5 | Kimi | Kimi share | Cost | ...
        if cells.len() < 9 {
            bail!("report row for group {} has too few columns", g.group);
        }

        let (p, t) = cells[2]
            .split_once('/')
            .with_context(|| format!("group {}: bad Passed cell {:?}", g.group, cells[2]))?;
        let p: u32 = p.trim().parse().context("parsing Passed")?;
        let t: u32 = t.trim().parse().context("parsing Total")?;
        if p != g.passed || t != g.total {
            bail!(
                "group {}: report table says {p}/{t}, results.json says {}/{}",
                g.group,
                g.passed,
                g.total
            );
        }
        close(&g.group, "report Score", num(cells[3])?, g.score_pct, 0.01)?;
        close(
            &g.group,
            "report Total requests",
            num(cells[4])?,
            f64::from(g.total_requests),
            0.5,
        )?;
        close(
            &g.group,
            "report GPT-5.5",
            num(cells[5])?,
            f64::from(g.strong_calls),
            0.5,
        )?;
        close(
            &g.group,
            "report Kimi",
            num(cells[6])?,
            f64::from(g.weak_calls),
            0.5,
        )?;
        close(
            &g.group,
            "report Kimi share",
            num(cells[7])?,
            g.weak_share_pct,
            0.01,
        )?;
        close(&g.group, "report Cost", num(cells[8])?, g.cost_usd, 0.005)?;
    }
    Ok(())
}

/// Count `(total_rows, passing_rows)` in a `benchmark-outcomes.jsonl` body,
/// where a passing row has `reward == 1.0`.
fn count_outcomes(jsonl: &str) -> Result<(u32, u32)> {
    let mut total = 0u32;
    let mut passed = 0u32;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let row: serde_json::Value =
            serde_json::from_str(line).context("parsing a benchmark-outcome row")?;
        total += 1;
        if let Some(reward) = row.get("reward").and_then(serde_json::Value::as_f64)
            && (reward - 1.0).abs() < 1e-9
        {
            passed += 1;
        }
    }
    Ok((total, passed))
}

fn pct(part: u32, whole: u32) -> f64 {
    f64::from(part) / f64::from(whole) * 100.0
}

/// Parse a numeric table cell, tolerating `$`, thousands separators, `%`, and spaces.
fn num(cell: &str) -> Result<f64> {
    let cleaned: String = cell
        .chars()
        .filter(|c| !matches!(c, '$' | ',' | '%' | ' '))
        .collect();
    cleaned
        .parse::<f64>()
        .with_context(|| format!("parsing numeric table cell {cell:?}"))
}

fn close(group: &str, field: &str, computed: f64, stated: f64, tol: f64) -> Result<()> {
    if (computed - stated).abs() > tol {
        bail!(
            "group {group}: {field} computed as {computed}, but file states {stated} (tolerance {tol})"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two groups: a control and one policy round, with figures that all
    // recompute cleanly (mirrors the shape of a real results.json).
    const RESULTS: &str = r#"{
      "run_id": "run-xyz",
      "comparable_task_count": 88,
      "groups": [
        {"group":"control","passed":68,"total":88,"score_pct":77.27,"total_requests":1666,
         "strong_calls":1666,"weak_calls":0,"weak_share_pct":0.0,"cost_usd":330.70,
         "cost_per_success_usd":4.86,"cost_vs_control_pct":0.0,"score_vs_control_pp":0.0},
        {"group":"r2","passed":67,"total":88,"score_pct":76.14,"total_requests":1512,
         "strong_calls":1293,"weak_calls":219,"weak_share_pct":14.48,"cost_usd":222.22,
         "cost_per_success_usd":3.32,"cost_vs_control_pct":-32.80,"score_vs_control_pp":-1.14}
      ],
      "lifecycle": {"policy_total_cost_usd":222.22,"policy_aggregate_successes":67}
    }"#;

    const REPORT: &str = "\
| Group | Passed | Score | Total requests | GPT-5.5 | Kimi | Kimi share | Cost | Cost vs control | Score vs control |\n\
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n\
| control | 68/88 | 77.27% | 1,666 | 1,666 | 0 | 0.00% | $330.70 | baseline | baseline |\n\
| r2 | 67/88 | 76.14% | 1,512 | 1,293 | 219 | 14.48% | $222.22 | -32.80% | -1.14 pp |\n\
\nrun-xyz\n";

    fn sample() -> Results {
        serde_json::from_str(RESULTS).unwrap()
    }

    #[test]
    fn valid_results_recompute() {
        validate_results(&sample()).unwrap();
    }

    #[test]
    fn mismatched_score_is_rejected() {
        let mut r = sample();
        r.groups[0].score_pct = 80.0;
        assert!(validate_results(&r).is_err());
    }

    #[test]
    fn call_split_must_sum_to_total_requests() {
        let mut r = sample();
        r.groups[1].weak_calls = 218;
        assert!(validate_results(&r).is_err());
    }

    #[test]
    fn lifecycle_totals_are_checked() {
        let mut r = sample();
        r.lifecycle.policy_total_cost_usd = 999.0;
        assert!(validate_results(&r).is_err());
    }

    #[test]
    fn report_table_matches_results() {
        check_report_table(REPORT, &sample()).unwrap();
    }

    #[test]
    fn report_table_mismatch_is_rejected() {
        let altered = REPORT.replace("$222.22", "$999.99");
        assert!(check_report_table(&altered, &sample()).is_err());
    }

    #[test]
    fn report_passed_mismatch_is_rejected() {
        let altered = REPORT.replace("67/88", "60/88");
        assert!(check_report_table(&altered, &sample()).is_err());
    }

    #[test]
    fn outcomes_are_counted() {
        let jsonl = "\
{\"task_id\":\"a\",\"reward\":1.0}\n\
{\"task_id\":\"b\",\"reward\":0.0}\n\
\n\
{\"task_id\":\"c\",\"reward\":1.0}\n";
        assert_eq!(count_outcomes(jsonl).unwrap(), (3, 2));
    }
}
