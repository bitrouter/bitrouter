//! Local-only, versioned read model for the independent menu-bar companion.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::daemon::{self, DaemonCommand, DaemonResponse};
use crate::metering::MeteringStore;
use crate::metering::companion::{ClientIdentity, CompanionUsageAggregate, UsageAggregate};
use crate::output::CliReport;
use crate::output::human::Human;
use crate::panel_activity::{AgentActivityEvent, AgentActivitySnapshot, AgentLifecycleState};

/// Explicit local-day bounds and per-client session page.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelInput {
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub session_limit: usize,
    pub session_offset: usize,
}

impl PanelInput {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(self.since < self.until, "panel since must precede until");
        anyhow::ensure!(
            self.until.signed_duration_since(self.since) <= chrono::Duration::hours(26),
            "panel time range must not exceed one local day (26 hours)"
        );
        anyhow::ensure!(
            (1..=500).contains(&self.session_limit),
            "panel session limit must be 1..=500"
        );
        anyhow::ensure!(
            self.session_offset <= 1_000_000,
            "panel session offset is too large"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelReport {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub usage_updated_at: DateTime<Utc>,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub clients: Vec<PanelClient>,
    #[serde(default)]
    pub agents: Vec<PanelAgent>,
    #[serde(default)]
    pub agent_events: Vec<PanelAgentEvent>,
    pub session_page: SessionPage,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelAgent {
    pub id: String,
    pub agent_id: String,
    pub label: String,
    pub session_id: String,
    pub short_id: String,
    pub state: AgentLifecycleState,
    pub activity: String,
    pub updated_at: DateTime<Utc>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelAgentEvent {
    pub id: String,
    pub agent_id: String,
    pub label: String,
    pub session_id: String,
    pub short_id: String,
    pub state: AgentLifecycleState,
    pub activity: String,
    pub occurred_at: DateTime<Utc>,
    pub source: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPage {
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
    pub next_offset: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelClient {
    pub id: String,
    pub label: String,
    pub tokens: TokenValue,
    pub last_activity_at: DateTime<Utc>,
    pub sessions: Vec<PanelSession>,
    pub accounts: Vec<PanelAccount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelSession {
    pub id: String,
    pub short_id: String,
    pub tokens: TokenValue,
    pub last_activity_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenValue {
    pub value: Option<u64>,
    pub state: String,
    pub has_unknown: bool,
}

impl From<&UsageAggregate> for TokenValue {
    fn from(usage: &UsageAggregate) -> Self {
        let all_unknown =
            usage.request_count > 0 && usage.provenance.unknown == usage.request_count;
        Self {
            value: (!all_unknown).then_some(usage.known_total_tokens),
            state: if all_unknown {
                "unknown"
            } else if usage.provenance.estimated > 0 {
                "estimated"
            } else {
                "known"
            }
            .into(),
            has_unknown: usage.provenance.unknown > 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelAccount {
    pub id: Option<String>,
    pub label: Option<String>,
    pub shared: bool,
    pub mapping_state: String,
    pub quota: PanelQuota,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PanelQuota {
    pub state: String,
    pub sampled_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
    pub windows: Vec<QuotaWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub label: String,
    pub remaining_percent: Option<f64>,
    pub remaining_tokens: Option<u64>,
    pub remaining_requests: Option<u64>,
    pub remaining_currency: Option<f64>,
    pub currency: Option<String>,
    pub resets_at: Option<DateTime<Utc>>,
    pub reset_kind: Option<String>,
}

/// Read through the owner-scoped daemon; never fall back to a different store.
pub async fn read(socket: &Path, input: PanelInput) -> Result<PanelReport> {
    input.validate()?;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        daemon::send_command(socket, &DaemonCommand::Panel { input }),
    )
    .await
    .context("panel_timeout: local BitRouter did not respond")?;
    match result {
        Ok(DaemonResponse::Panel { report }) => Ok(*report),
        Ok(DaemonResponse::Error { message }) if message.starts_with("invalid command:") => {
            anyhow::bail!(
                "panel_unsupported_daemon: restart with a BitRouter version supporting panel"
            )
        }
        Ok(DaemonResponse::Error { message }) => anyhow::bail!("{message}"),
        Ok(_) => anyhow::bail!("panel_unsupported_daemon: unexpected local response"),
        Err(_) => anyhow::bail!("panel_daemon_unavailable: start the local BitRouter daemon"),
    }
}

pub async fn report(
    store: &MeteringStore,
    input: PanelInput,
    quotas: Option<&crate::panel_quota::PanelQuotaService>,
    activities: &[AgentActivitySnapshot],
    activity_events: &[AgentActivityEvent],
) -> Result<PanelReport> {
    input.validate()?;
    let usage = store
        .aggregate_companion_snapshot(input.since, input.until, input.session_offset > 0)
        .await?;
    let account_refs = usage
        .clients
        .iter()
        .flat_map(|client| &client.upstream_sources)
        .filter(|source| source.provider_id == "openai-codex")
        .filter_map(|source| source.upstream_account_ref.clone())
        .collect::<Vec<_>>();
    let snapshots = quotas
        .map(|service| service.snapshot(&account_refs))
        .unwrap_or_default();
    from_usage(usage, &input, &snapshots, activities, activity_events)
}

fn opaque_id(value: &impl Serialize) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(value)?)))
}

fn from_usage(
    usage: CompanionUsageAggregate,
    input: &PanelInput,
    quotas: &HashMap<String, PanelQuota>,
    activities: &[AgentActivitySnapshot],
    activity_events: &[AgentActivityEvent],
) -> Result<PanelReport> {
    let mut sharing = BTreeMap::<String, usize>::new();
    for client in &usage.clients {
        for source in &client.upstream_sources {
            if let Some(account_ref) = &source.upstream_account_ref {
                *sharing.entry(account_ref.clone()).or_default() += 1;
            }
        }
    }
    let account_numbers = sharing
        .keys()
        .enumerate()
        .map(|(index, id)| (id.clone(), index + 1))
        .collect::<BTreeMap<_, _>>();
    let mut clients = Vec::with_capacity(usage.clients.len());
    let mut has_more = false;
    for client in usage.clients {
        let id = opaque_id(&client.client)?;
        let label = match &client.client {
            ClientIdentity::Known { harness } => match harness.as_str() {
                "codex" => "Codex".into(),
                "claude_code" | "claude-code" => "Claude Code".into(),
                "opencode" => "OpenCode".into(),
                other => other.chars().filter(|c| !c.is_control()).take(80).collect(),
            },
            ClientIdentity::Unknown => "未识别客户端".into(),
        };
        let assigned_count = client.sessions.len();
        let session_count = assigned_count + usize::from(client.unassigned.is_some());
        let page_end = input.session_offset + input.session_limit;
        has_more |= session_count > page_end;
        let mut sessions = Vec::with_capacity(input.session_limit.min(session_count));
        for session in client
            .sessions
            .into_iter()
            .skip(input.session_offset)
            .take(input.session_limit)
        {
            let session_id = opaque_id(&(&id, &session.session))?;
            sessions.push(PanelSession {
                short_id: session_id.chars().take(8).collect(),
                id: session_id,
                tokens: (&session.usage).into(),
                last_activity_at: Some(session.latest_activity_at),
            });
        }
        if let Some(unassigned) = &client.unassigned
            && assigned_count >= input.session_offset
            && assigned_count < page_end
        {
            sessions.push(PanelSession {
                id: format!("{id}:unassigned"),
                short_id: "未归属会话".into(),
                tokens: unassigned.into(),
                last_activity_at: None,
            });
        }
        let accounts = client
            .upstream_sources
            .iter()
            .map(|source| {
                let known = source.upstream_account_ref.is_some();
                let label = match source.provider_id.as_str() {
                    "openai-codex" => "Codex",
                    "claude-code" => "Claude",
                    provider => provider,
                };
                let display = source
                    .upstream_account_ref
                    .as_ref()
                    .and_then(|id| account_numbers.get(id))
                    .map_or_else(
                        || format!("{label} · 账户未知"),
                        |n| format!("{label} · 账户 {n}"),
                    );
                let quota = source
                    .upstream_account_ref
                    .as_ref()
                    .and_then(|id| quotas.get(id))
                    .cloned()
                    .unwrap_or_else(|| PanelQuota {
                        state: if !known {
                            "unknown"
                        } else if source.provider_id == "openai-codex" {
                            "error"
                        } else {
                            "unsupported"
                        }
                        .into(),
                        sampled_at: None,
                        error: if source.provider_id == "claude-code" {
                            Some("claude_quota_unavailable".into())
                        } else {
                            None
                        },
                        windows: vec![],
                    });
                PanelAccount {
                    id: source.upstream_account_ref.clone(),
                    label: Some(display),
                    shared: source
                        .upstream_account_ref
                        .as_ref()
                        .and_then(|id| sharing.get(id))
                        .is_some_and(|n| *n > 1),
                    mapping_state: if known { "known" } else { "unknown" }.into(),
                    quota,
                }
            })
            .collect();
        clients.push(PanelClient {
            id,
            label,
            tokens: (&client.usage).into(),
            last_activity_at: client.latest_activity_at,
            sessions,
            accounts,
        });
    }
    let sampled = Utc::now();
    let agents = activities
        .iter()
        .map(panel_agent)
        .collect::<Result<Vec<_>>>()?;
    let agent_events = activity_events
        .iter()
        .map(panel_agent_event)
        .collect::<Result<Vec<_>>>()?;
    Ok(PanelReport {
        schema_version: 1,
        generated_at: sampled,
        usage_updated_at: sampled,
        since: input.since,
        until: input.until,
        clients,
        agents,
        agent_events,
        session_page: SessionPage {
            offset: input.session_offset,
            limit: input.session_limit,
            has_more,
            next_offset: has_more.then_some(input.session_offset + input.session_limit),
        },
        warnings: vec![],
    })
}

fn panel_agent(activity: &AgentActivitySnapshot) -> Result<PanelAgent> {
    let session_id = opaque_id(&(&activity.agent_id, &activity.session_id))?;
    Ok(PanelAgent {
        id: opaque_id(&activity.instance_id)?,
        agent_id: activity.agent_id.clone(),
        label: agent_label(&activity.agent_id),
        short_id: session_id.chars().take(8).collect(),
        session_id,
        state: activity.state,
        activity: activity.activity.clone(),
        updated_at: activity.updated_at,
        source: "managed_acp".into(),
    })
}

fn panel_agent_event(event: &AgentActivityEvent) -> Result<PanelAgentEvent> {
    let session_id = opaque_id(&(&event.agent_id, &event.session_id))?;
    Ok(PanelAgentEvent {
        id: event.id.clone(),
        agent_id: event.agent_id.clone(),
        label: agent_label(&event.agent_id),
        short_id: session_id.chars().take(8).collect(),
        session_id,
        state: event.state,
        activity: event.activity.clone(),
        occurred_at: event.occurred_at,
        source: "managed_acp".into(),
    })
}

fn agent_label(agent_id: &str) -> String {
    match agent_id {
        "codex" | "codex-acp" => "Codex".into(),
        "claude" | "claude-code" | "claude-code-acp" => "Claude Code".into(),
        "opencode" | "opencode-acp" => "OpenCode".into(),
        other => other
            .trim_end_matches("-acp")
            .chars()
            .filter(|character| !character.is_control())
            .take(80)
            .collect(),
    }
}

impl CliReport for PanelReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.line("BitRouter panel — settled token usage")?;
        for client in &self.clients {
            let tokens = client
                .tokens
                .value
                .map_or_else(|| "unknown".into(), |n| n.to_string());
            human.line(&format!(
                "{}: {} tokens{}",
                client.label,
                tokens,
                if client.tokens.has_unknown {
                    " (partial)"
                } else {
                    ""
                }
            ))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_accounts_do_not_supply_quota_to_unknown_rows() -> Result<()> {
        use crate::metering::companion::{ClientUsage, UpstreamUsageSource};

        let until = Utc::now();
        let since = until - chrono::Duration::hours(1);
        let source = |account: Option<&str>| UpstreamUsageSource {
            provider_id: "openai-codex".into(),
            upstream_account_ref: account.map(str::to_owned),
        };
        let client = |harness: &str, upstream_sources| ClientUsage {
            client: ClientIdentity::Known {
                harness: harness.into(),
            },
            usage: UsageAggregate::default(),
            upstream_sources,
            sessions: Vec::new(),
            unassigned: None,
            latest_activity_at: until,
        };
        let quota = PanelQuota {
            state: "available".into(),
            sampled_at: Some(until),
            error: None,
            windows: vec![],
        };
        let report = from_usage(
            CompanionUsageAggregate {
                since,
                until,
                clients: vec![
                    client("codex", vec![source(Some("account-a")), source(None)]),
                    client("claude-code", vec![source(Some("account-a"))]),
                ],
            },
            &PanelInput {
                since,
                until,
                session_limit: 100,
                session_offset: 0,
            },
            &HashMap::from([("account-a".into(), quota)]),
            &[],
            &[],
        )?;
        let known = report
            .clients
            .iter()
            .flat_map(|client| &client.accounts)
            .filter(|account| account.id.is_some())
            .collect::<Vec<_>>();
        assert_eq!(known.len(), 2);
        assert!(
            known
                .iter()
                .all(|account| account.shared && account.quota.state == "available")
        );
        let unknown = report
            .clients
            .iter()
            .flat_map(|client| &client.accounts)
            .find(|account| account.id.is_none())
            .context("unknown account missing")?;
        assert_eq!(unknown.mapping_state, "unknown");
        assert_eq!(unknown.quota.state, "unknown");
        assert!(!unknown.shared);
        assert!(unknown.quota.sampled_at.is_none());
        Ok(())
    }

    #[test]
    fn managed_activity_is_opaque_in_panel_output() -> Result<()> {
        let now = Utc::now();
        let activity = AgentActivitySnapshot {
            instance_id: "process-secret".into(),
            agent_id: "codex-acp".into(),
            session_id: "native-session-secret".into(),
            state: AgentLifecycleState::NeedsApproval,
            activity: "Approval needed".into(),
            updated_at: now,
            last_seen_at: now,
        };
        let panel = panel_agent(&activity)?;
        assert_eq!(panel.label, "Codex");
        assert_eq!(panel.state, AgentLifecycleState::NeedsApproval);
        assert_ne!(panel.id, activity.instance_id);
        assert_ne!(panel.session_id, activity.session_id);
        assert_eq!(panel.short_id.len(), 8);
        Ok(())
    }

    #[test]
    fn additive_activity_fields_accept_older_panel_responses() -> Result<()> {
        let now = Utc::now();
        let value = serde_json::json!({
            "schema_version": 1,
            "generated_at": now,
            "usage_updated_at": now,
            "since": now - chrono::Duration::minutes(1),
            "until": now,
            "clients": [],
            "session_page": {
                "offset": 0,
                "limit": 100,
                "has_more": false,
                "next_offset": null
            },
            "warnings": []
        });
        let report: PanelReport = serde_json::from_value(value)?;
        assert!(report.agents.is_empty());
        assert!(report.agent_events.is_empty());
        Ok(())
    }

    #[test]
    fn unknown_is_not_a_zero_reading() {
        let mut usage = UsageAggregate {
            request_count: 1,
            ..Default::default()
        };
        usage.provenance.unknown = 1;
        let value = TokenValue::from(&usage);
        assert_eq!(value.value, None);
        assert_eq!(value.state, "unknown");
        assert!(value.has_unknown);
    }

    #[test]
    fn partial_estimate_keeps_both_qualifiers() {
        let mut usage = UsageAggregate {
            request_count: 2,
            known_total_tokens: 17,
            ..Default::default()
        };
        usage.provenance.unknown = 1;
        usage.provenance.estimated = 1;
        let value = TokenValue::from(&usage);
        assert_eq!(value.value, Some(17));
        assert_eq!(value.state, "estimated");
        assert!(value.has_unknown);
    }
}
