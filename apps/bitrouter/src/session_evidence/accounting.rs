//! Unique gateway request sets, kept separate from transcript event dedup.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use sea_orm::{
    ColumnTrait, Condition, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, QuerySelect,
};
use serde::{Deserialize, Serialize};

use super::types::{Harness, MAX_GRAPH_ITEMS, MAX_RECORDS, NodeKey};
use crate::eval::store::EvalStore;
use crate::eval::types::EvalDecisionRef;
use crate::metering::entities::requests;
use crate::metering::pricing::ChargeStatus;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestEvidence {
    pub request_ids: BTreeSet<String>,
    pub decisions: Vec<EvalDecisionRef>,
    pub decision_requests: BTreeMap<String, String>,
    pub cost_micro_usd: Option<i64>,
    pub latency_ms: i64,
    pub gaps: BTreeSet<String>,
}

#[derive(sea_orm::FromQueryResult)]
struct Row {
    request_id: String,
    user_id: String,
    route_scope_id: Option<String>,
    estimated_charge_micro_usd: i64,
    charge_status: String,
    latency_ms: i64,
    agent_harness: Option<String>,
    native_root_session_id: Option<String>,
    native_agent_thread_id: Option<String>,
}

pub async fn collect(
    db: &DatabaseConnection,
    scope: Option<&str>,
    members: &BTreeSet<NodeKey>,
    excluded: &BTreeSet<String>,
) -> Result<RequestEvidence> {
    let mut evidence = RequestEvidence::default();
    let Some(scope) = scope else {
        evidence
            .gaps
            .insert("gateway_attribution_unavailable".into());
        return Ok(evidence);
    };
    ensure!(
        !members.is_empty() && members.len() <= MAX_GRAPH_ITEMS,
        "invalid request membership"
    );
    let mut nodes = Condition::any();
    for node in members {
        node.validate()?;
        let mut native =
            Condition::all().add(requests::Column::AgentHarness.eq(harness_name(node.harness)));
        match node.harness {
            Harness::Codex => {
                native = native.add(requests::Column::NativeAgentThreadId.eq(&node.native_id));
            }
            Harness::ClaudeCode => {
                native = native.add(requests::Column::NativeRootSessionId.eq(&node.native_id));
                native = native.add(match &node.agent_id {
                    Some(agent) => requests::Column::NativeAgentThreadId.eq(agent),
                    None => requests::Column::NativeAgentThreadId.is_null(),
                });
            }
        }
        nodes = nodes.add(native);
    }
    let store = EvalStore::new(db.clone());
    let mut after = None;
    let mut scanned = 0;
    let mut cost = 0i64;
    let mut known_cost = true;
    loop {
        let mut query = requests::Entity::find()
            .select_only()
            .columns([
                requests::Column::RequestId,
                requests::Column::UserId,
                requests::Column::RouteScopeId,
                requests::Column::EstimatedChargeMicroUsd,
                requests::Column::ChargeStatus,
                requests::Column::LatencyMs,
                requests::Column::AgentHarness,
                requests::Column::NativeRootSessionId,
                requests::Column::NativeAgentThreadId,
            ])
            .filter(requests::Column::RouteScopeId.eq(scope))
            .filter(nodes.clone())
            .order_by_asc(requests::Column::RequestId)
            .limit(128);
        if let Some(id) = &after {
            query = query.filter(requests::Column::RequestId.gt(id));
        }
        let rows = query.into_model::<Row>().all(db).await?;
        if rows.is_empty() {
            break;
        }
        for row in &rows {
            scanned += 1;
            ensure!(scanned <= MAX_RECORDS, "native request scan exceeds limit");
            // Verify exact Rust equality as well as the database predicate so
            // MySQL's default case-insensitive collation cannot broaden scope.
            if row.route_scope_id.as_deref() != Some(scope)
                || !members.iter().any(|node| matches_node(row, node))
            {
                continue;
            }
            if excluded.contains(&row.request_id) {
                continue;
            }
            ensure!(
                evidence.request_ids.insert(row.request_id.clone()),
                "duplicate gateway request id"
            );
            match ChargeStatus::from_persisted(&row.charge_status) {
                ChargeStatus::Computed | ChargeStatus::NotCharged => {
                    cost = cost
                        .checked_add(row.estimated_charge_micro_usd)
                        .ok_or_else(|| anyhow::anyhow!("request cost overflow"))?;
                }
                _ => known_cost = false,
            }
            evidence.latency_ms = evidence
                .latency_ms
                .checked_add(row.latency_ms)
                .ok_or_else(|| anyhow::anyhow!("request latency overflow"))?;
            if let Some(subject) = store
                .subject_for_owner(&format!("request:{}", row.request_id), &row.user_id)
                .await?
            {
                for decision in subject.decisions {
                    if evidence
                        .decision_requests
                        .insert(decision.decision_id.clone(), row.request_id.clone())
                        .is_some()
                    {
                        anyhow::bail!("routing decision belongs to multiple requests");
                    }
                    evidence.decisions.push(decision);
                }
            } else {
                evidence.gaps.insert("request_decision_unavailable".into());
            }
        }
        after = rows.last().map(|row| row.request_id.clone());
        if rows.len() < 128 {
            break;
        }
    }
    if evidence.request_ids.is_empty() {
        evidence.gaps.insert("gateway_request_set_empty".into());
    }
    if !known_cost {
        evidence.gaps.insert("request_charge_unknown".into());
    }
    evidence.cost_micro_usd = known_cost.then_some(cost);
    evidence
        .decisions
        .sort_by(|left, right| left.decision_id.cmp(&right.decision_id));
    Ok(evidence)
}

fn harness_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Codex => "codex",
        Harness::ClaudeCode => "claude_code",
    }
}

fn matches_node(row: &Row, node: &NodeKey) -> bool {
    row.agent_harness.as_deref() == Some(harness_name(node.harness))
        && match node.harness {
            Harness::Codex => {
                row.native_agent_thread_id.as_deref() == Some(node.native_id.as_str())
            }
            Harness::ClaudeCode => {
                row.native_root_session_id.as_deref() == Some(node.native_id.as_str())
                    && row.native_agent_thread_id == node.agent_id
            }
        }
}
