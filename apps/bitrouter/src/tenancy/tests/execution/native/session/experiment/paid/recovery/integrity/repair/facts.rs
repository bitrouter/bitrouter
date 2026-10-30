//! Exact original source and allowed terminal delta; no positive SQL construction.
use super::*;

fn column(table: &Value, name: &str) -> Result<usize> {
    table["columns"]
        .as_array()
        .context("snapshot columns")?
        .iter()
        .position(|value| value == name)
        .context("snapshot column absent")
}
fn cells(table: &Value) -> Result<&[Value]> {
    Ok(table["rows"]
        .as_array()
        .context("snapshot rows")?
        .as_slice())
}
fn text(cell: &Value) -> Result<String> {
    let raw = cell.as_str().context("quoted SQL cell")?;
    let quoted = raw
        .strip_prefix('\'')
        .and_then(|v| v.strip_suffix('\''))
        .context("expected SQL TEXT")?;
    Ok(quoted.replace("''", "'"))
}

pub(super) fn interrupted(
    before: &state::Snapshot,
    after: &state::Snapshot,
    original: &Value,
) -> Result<()> {
    let a = serde_json::to_value(before)?;
    let b = serde_json::to_value(after)?;
    ensure!(a["schema"] == b["schema"]);
    let tables = a["tables"].as_object().context("original tables")?;
    let changed = b["tables"].as_object().context("repaired tables")?;
    ensure!(tables.keys().eq(changed.keys()));
    for (name, old) in tables {
        let new = changed.get(name).context("preserved table")?;
        ensure!(old["definition"] == new["definition"] && old["columns"] == new["columns"]);
        if !matches!(
            name.as_str(),
            "checkpoint_evolution" | "tenancy_execution_receipts"
        ) {
            ensure!(
                old == new,
                "repair changed native source/head/history/table {name}"
            );
        }
    }
    let old = &tables["checkpoint_evolution"];
    let new = &changed["checkpoint_evolution"];
    let id = column(old, "record_id")?;
    let kind = column(old, "kind")?;
    let revision = column(old, "revision")?;
    let body = column(old, "body")?;
    let old_rows = cells(old)?;
    let new_rows = cells(new)?;
    ensure!(old_rows.len() == new_rows.len() + 1);
    let mut terminals = 0;
    let mut pending = 0;
    let mut counters = 0;
    for row in old_rows {
        let row_kind = text(&row[kind])?;
        let next = new_rows.iter().find(|next| next[id] == row[id]);
        if row_kind == "owned_judge_pending" {
            pending += 1;
            ensure!(
                serde_json::from_str::<Value>(&text(&row[body])?)? == *original && next.is_none()
            );
            continue;
        }
        let next = next.context("original row removed")?;
        if row_kind == "owned_judge_active_v3" {
            terminals += 1;
            ensure!(serde_json::from_str::<Value>(&text(&row[body])?)? == *original);
            ensure!(text(&next[kind])? == "owned_judge_job");
            ensure!(
                next[revision]
                    .as_str()
                    .context("revision")?
                    .parse::<i64>()?
                    == row[revision]
                        .as_str()
                        .context("old revision")?
                        .parse::<i64>()?
                        + 1
            );
            let mut expected = original.clone();
            expected["status"] = json!("interrupted");
            expected["history_reservation"]["state"] = json!({"state":"released_without_revision"});
            ensure!(
                serde_json::from_str::<Value>(&text(&next[body])?)? == expected,
                "repair changed original seed/stopset/cleanup/evidence or invented output"
            );
            for (index, cell) in row.as_array().context("row")?.iter().enumerate() {
                if ![kind, revision, body].contains(&index) {
                    ensure!(cell == &next[index]);
                }
            }
        } else {
            if row_kind == "owned_judge_budget" {
                counters += 1;
                ensure!(serde_json::from_str::<Value>(&text(&row[body])?)?["accepted"] == 1);
            }
            ensure!(row == next, "repair changed counter/other original row");
        }
    }
    ensure!(terminals == 1 && pending == 1 && counters == 1);
    let old = &tables["tenancy_execution_receipts"];
    let new = &changed["tenancy_execution_receipts"];
    let execution = column(old, "execution_id")?;
    let old_rows = cells(old)?;
    let new_rows = cells(new)?;
    ensure!(new_rows.len() == old_rows.len() + 1);
    for row in old_rows {
        ensure!(new_rows.contains(row), "original035 bytes changed");
    }
    let additions: Vec<_> = new_rows
        .iter()
        .filter(|row| !old_rows.contains(row))
        .collect();
    ensure!(
        additions.len() == 1
            && text(&additions[0][execution])?
                == original["execution"]
                    .as_str()
                    .context("original paid execution")?
    );
    Ok(())
}

pub(super) async fn terminal(
    db: &DatabaseConnection,
    original: &Value,
) -> Result<crate::tenancy::receipts::store::Candidate> {
    let key = key(original)?;
    let candidate = crate::tenancy::receipts::store::lookup_candidate(
        db,
        &key.installation,
        &key.tenant,
        &key.execution,
    )
    .await?
    .context("actual repaired035")?;
    ensure!(
        candidate.experiment_disposition()?.1
            == bitrouter_ai::tenancy::operation::DispatchState::Unknown
            && !candidate.has_auxiliary(),
        "repair must retain an unknown original invocation, not invent attempts or auxiliary facts"
    );
    let receipt = &candidate.receipt;
    ensure!(
        receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Unknown
            && receipt.outcome == bitrouter_ai::tenancy::execution::Outcome::Unknown
            && receipt.attempts.is_empty()
            && receipt.usage.provenance == bitrouter_ai::tenancy::execution::Provenance::Unknown
            && !receipt.usage.complete
            && receipt.usage.input_tokens.is_none()
            && receipt.usage.output_tokens.is_none()
            && receipt.usage.cache_read_tokens.is_none()
            && receipt.usage.cache_write_tokens.is_none()
            && receipt.usage.reasoning_tokens.is_none()
    );
    let seed = &original["recovery"]["seed"];
    ensure!(
        receipt.installation_id == key.installation
            && receipt.tenant_id == key.tenant
            && receipt.execution_id == key.execution
    );
    ensure!(
        receipt.principal_id.as_str() == seed["principal"].as_str().context("principal")?
            && receipt.execution_revision.as_str()
                == seed["configuration"].as_str().context("configuration")?
            && receipt.correlation_id.as_str()
                == seed["correlation"].as_str().context("correlation")?
    );
    Ok(candidate)
}
