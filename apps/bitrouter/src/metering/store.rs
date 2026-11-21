//! `MeteringStore` — read+write access to the `requests` table.
//!
//! Single writer ([`super::MeteringRecorder`]), multiple readers (the
//! `policy` module's spend-cap enforcement; future analytics CLI).
//!
//! Every query goes through sea-orm, so the store works unchanged on
//! whichever backend `database.url` selects (SQLite / Postgres / MySQL).
//! The window rollups (`SUM` of charges / tokens) are folded in Rust
//! rather than as a SQL aggregate: `SUM` returns a different value type
//! per backend (`integer` / `numeric` / `decimal`), and folding the rows
//! sidesteps that entirely. The metering windows are bounded (at most one
//! month) so the row counts stay modest for a local-first deployment.

mod storage;

use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, DbBackend, EntityTrait,
    PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set, Statement,
};

use crate::gateway::error::{BitrouterError, Result};

use crate::metering::db::RequestMetric;
use crate::metering::entities::requests;
use crate::metering::pricing::ChargeStatus;

/// A rolling time window for usage queries.
#[derive(Debug, Clone, Copy)]
pub enum TimeWindow {
    /// Trailing 60 seconds.
    LastMinute,
    /// Since UTC 00:00 today.
    Today,
    /// Since UTC 00:00 on the 1st this month.
    ThisMonth,
    /// An explicit `[start, end)` range.
    Custom {
        /// Inclusive start.
        start: DateTime<Utc>,
        /// Exclusive end.
        end: DateTime<Utc>,
    },
}

/// Request / token rate over the trailing minute.
#[derive(Debug, Clone, Copy, Default)]
pub struct RateMetrics {
    /// Requests observed in the trailing minute.
    pub requests_per_minute: f64,
    /// Tokens observed in the trailing minute.
    pub tokens_per_minute: f64,
}

/// Spend / request rollup over a window, across **every** caller.
///
/// Read by the agent-facing CLI surfaces (`status --agent`,
/// the MCP cost footer, the `spawn` exit summary) — unlike the
/// per-key getters above, nothing here is
/// scoped to one `api_key_id`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpendSummary {
    /// Total estimated spend in micro-USD, counting **only** requests that
    /// carry charge evidence ([`ChargeStatus::Computed`] or
    /// [`ChargeStatus::NotCharged`]).
    ///
    /// Summing the unpriced rows too would report a floor as a price: a row
    /// whose evidence was incomplete stores `0`, which is indistinguishable
    /// from a genuinely free request once added. [`Self::unpriced`] is how a
    /// caller learns the figure is partial.
    pub spend_micro_usd: u64,
    /// Requests observed (success and failure alike).
    pub requests: u64,
    /// How many of `requests` have no charge evidence, and are therefore
    /// absent from `spend_micro_usd`.
    pub unpriced: u64,
}

/// One settled request in the bounded operator inspection view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRow {
    /// Request id — also the join key into the trajectory store.
    pub request_id: String,
    /// RFC3339 settle timestamp.
    pub created_at: String,
    /// Named router selected during Stage 0, when the request used one.
    pub router_id: Option<String>,
    /// Versioned digest of the non-secret router binding used by the request.
    pub binding_digest: Option<String>,
    /// Caller selector captured before ingress transforms.
    pub original_selector: Option<String>,
    /// Model the router resolved to.
    pub model_id: String,
    /// Provider that actually served the request.
    pub provider_id: String,
    /// Prompt tokens consumed.
    pub prompt_tokens: i64,
    /// Completion tokens produced.
    pub completion_tokens: i64,
    /// Cache-read prompt tokens.
    pub cache_read_tokens: i64,
    /// Cache-write prompt tokens.
    pub cache_write_tokens: i64,
    /// Estimated charge in micro-USD.
    pub estimated_charge_micro_usd: i64,
    /// End-to-end latency in milliseconds.
    pub latency_ms: i64,
    /// Error string when the request failed, else `None`.
    pub error: Option<String>,
    /// How the charge figure was arrived at.
    ///
    /// [`ChargeStatus::Computed`] and [`ChargeStatus::NotCharged`] are
    /// evidence; the two unknowns are not. A renderer must never turn an
    /// unknown into `$0.00` — the charge column stores `0` in that case, and
    /// a zero it did not measure is not a free request.
    pub charge_status: ChargeStatus,
    /// The trajectory episode this request belongs to, when trajectory
    /// capture recorded one.
    ///
    /// `None` when capture is off (it defaults to off and is restart-only),
    /// when the request predates it, or when the ledger is unreadable. This
    /// is the thread from a settled request to `bitrouter trajectory
    /// inspect`, which is otherwise reachable only by an episode id nothing
    /// hands out.
    pub episode_id: Option<String>,
}

/// One bounded page from the host-wide request inspection query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestPage {
    /// Newest-first rows, never exceeding the requested limit.
    pub rows: Vec<RequestRow>,
    /// A further matching row existed after this page.
    pub truncated: bool,
}

impl From<requests::Model> for RequestRow {
    fn from(m: requests::Model) -> Self {
        Self {
            request_id: m.request_id,
            created_at: m.created_at,
            router_id: m.router_id,
            binding_digest: m.binding_digest,
            original_selector: m.original_selector,
            model_id: m.model_id,
            provider_id: m.provider_id,
            prompt_tokens: m.prompt_tokens,
            completion_tokens: m.completion_tokens,
            cache_read_tokens: m.cache_read_tokens,
            cache_write_tokens: m.cache_write_tokens,
            estimated_charge_micro_usd: m.estimated_charge_micro_usd,
            latency_ms: m.latency_ms,
            error: m.error,
            charge_status: ChargeStatus::from_persisted(&m.charge_status),
            // Filled by `attach_episodes`; the row itself has no join.
            episode_id: None,
        }
    }
}

/// Fold `(charge, persisted charge_status)` pairs into a summary.
///
/// Shared by both spend queries so they cannot drift on what counts as
/// evidence. A row without evidence contributes to `requests` and `unpriced`
/// but **not** to the total: its stored charge is `0`, and adding that would
/// silently turn "we do not know" into "it was free".
fn summarize(charges: Vec<(i64, String)>) -> SpendSummary {
    let mut summary = SpendSummary {
        requests: charges.len() as u64,
        ..Default::default()
    };
    for (charge, status) in charges {
        match ChargeStatus::from_persisted(&status) {
            ChargeStatus::Computed | ChargeStatus::NotCharged => {
                summary.spend_micro_usd += charge.max(0) as u64;
            }
            ChargeStatus::Unknown | ChargeStatus::LegacyUnknown => summary.unpriced += 1,
        }
    }
    summary
}

/// sea-orm-backed metering store.
#[derive(Clone)]
pub struct MeteringStore {
    pub(super) db: DatabaseConnection,
    storage_budget: Option<Arc<OnceLock<crate::tenancy::storage::Budget>>>,
}

impl MeteringStore {
    /// Build a store over a database connection. The database must already
    /// carry the `requests` table (`crate::db::run_migrations`).
    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            storage_budget: None,
        }
    }

    /// Attach the owned database's shared budget before cloning the store.
    /// Writes refuse admission until the tenancy service fills this cell.
    pub(crate) fn with_storage_budget(
        mut self,
        budget: Arc<OnceLock<crate::tenancy::storage::Budget>>,
    ) -> Self {
        self.storage_budget = Some(budget);
        self
    }

    /// Spend suitable for enforcing a hard policy ceiling.
    ///
    /// Enforcement fails closed whenever any row in the window has
    /// unknown or legacy charge evidence.
    pub async fn get_enforceable_spend(
        &self,
        api_key_id: &str,
        window: TimeWindow,
    ) -> Result<Option<u64>> {
        let start = window_start(window).to_rfc3339();
        let charges: Vec<(i64, String)> = requests::Entity::find()
            .select_only()
            .column(requests::Column::EstimatedChargeMicroUsd)
            .column(requests::Column::ChargeStatus)
            .filter(requests::Column::ApiKeyId.eq(api_key_id))
            .filter(requests::Column::CreatedAt.gte(start))
            .into_tuple()
            .all(&self.db)
            .await
            .map_err(|error| BitrouterError::internal(format!("get_enforceable_spend: {error}")))?;
        let mut total = 0_u64;
        for (charge, status) in charges {
            match ChargeStatus::from_persisted(&status) {
                ChargeStatus::Computed | ChargeStatus::NotCharged => {
                    total = total.saturating_add(charge.max(0) as u64);
                }
                ChargeStatus::Unknown | ChargeStatus::LegacyUnknown => return Ok(None),
            }
        }
        Ok(Some(total))
    }

    /// Current request/token rate for `api_key_id`.
    pub async fn get_rate(&self, api_key_id: &str) -> Result<RateMetrics> {
        let start = window_start(TimeWindow::LastMinute).to_rfc3339();
        let rows: Vec<(i64, i64)> = requests::Entity::find()
            .select_only()
            .column(requests::Column::PromptTokens)
            .column(requests::Column::CompletionTokens)
            .filter(requests::Column::ApiKeyId.eq(api_key_id))
            .filter(requests::Column::CreatedAt.gte(start))
            .into_tuple()
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("get_rate: {e}")))?;
        let tokens: u64 = rows
            .iter()
            .map(|(p, c)| (*p).max(0) as u64 + (*c).max(0) as u64)
            .sum();
        Ok(RateMetrics {
            requests_per_minute: rows.len() as f64,
            tokens_per_minute: tokens as f64,
        })
    }

    /// The newest host-wide rows matching the supplied time/model/provider
    /// filters. Filters are applied in the database before the page limit, and
    /// one extra row makes truncation observable without an unbounded scan.
    pub async fn recent_requests_filtered(
        &self,
        window: TimeWindow,
        model: Option<&str>,
        provider: Option<&str>,
        limit: u64,
    ) -> Result<RequestPage> {
        let start = window_start(window).to_rfc3339();
        let mut query = requests::Entity::find()
            .filter(requests::Column::CreatedAt.gte(start))
            .order_by_desc(requests::Column::CreatedAt)
            .order_by_desc(requests::Column::RequestId);
        if let Some(model) = model {
            query = query.filter(requests::Column::ModelId.eq(model));
        }
        if let Some(provider) = provider {
            query = query.filter(requests::Column::ProviderId.eq(provider));
        }
        if let TimeWindow::Custom { end, .. } = window {
            query = query.filter(requests::Column::CreatedAt.lt(end.to_rfc3339()));
        }
        let mut rows = query
            .limit(limit.saturating_add(1))
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("recent_requests_filtered: {e}")))?;
        let truncated = rows.len() > limit as usize;
        if truncated {
            rows.pop();
        }
        let mut rows: Vec<RequestRow> = rows.into_iter().map(RequestRow::from).collect();
        self.attach_episodes(&mut rows).await;
        Ok(RequestPage { rows, truncated })
    }

    /// Fill in each row's trajectory episode id, where one exists.
    ///
    /// A second statement rather than a join: `trajectory_requests` belongs to
    /// the trajectory ledger and has no entity here, and this store must not
    /// grow a dependency on that module merely to answer "is there more to
    /// read about this request". The two tables share one database, and
    /// `request_id` is the documented join key.
    ///
    /// **Best effort, by design.** Trajectory capture defaults to off, so the
    /// common case is a table that exists and is empty. A database old enough
    /// to lack the table entirely, or any other read failure, leaves every row
    /// `None` — the same answer as "capture was off", which is the honest
    /// reading either way. It must never fail the request table over a
    /// ledger that is optional.
    async fn attach_episodes(&self, rows: &mut [RequestRow]) {
        if rows.is_empty() {
            return;
        }
        let backend = self.db.get_database_backend();
        let placeholders: Vec<String> = (1..=rows.len())
            .map(|i| match backend {
                DbBackend::Postgres => format!("${i}"),
                _ => "?".to_string(),
            })
            .collect();
        let sql = format!(
            "SELECT request_id, episode_id FROM trajectory_requests WHERE installation_id IS NULL AND tenant_id IS NULL \
             AND creator_principal_id IS NULL AND logical_identity_json IS NULL AND request_id IN ({})",
            placeholders.join(", ")
        );
        let values: Vec<sea_orm::Value> = rows
            .iter()
            .map(|r| sea_orm::Value::from(r.request_id.clone()))
            .collect();
        let Ok(found) = self
            .db
            .query_all(Statement::from_sql_and_values(backend, &sql, values))
            .await
        else {
            return;
        };
        let mut episodes: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for row in found {
            if let (Ok(request_id), Ok(episode_id)) = (
                row.try_get::<String>("", "request_id"),
                row.try_get::<String>("", "episode_id"),
            ) {
                episodes.insert(request_id, episode_id);
            }
        }
        for row in rows.iter_mut() {
            row.episode_id = episodes.get(&row.request_id).cloned();
        }
    }

    /// Request / token rate over the trailing minute across **every** caller.
    ///
    /// [`Self::get_rate`] is scoped to one `api_key_id`, which cannot serve a
    /// machine-wide readout: under the `skip_auth: true` default every request
    /// is attributed to the synthetic `local` caller, but a daemon also serves
    /// real `brk_` keys and pre-auth `anonymous` ones, and the live view means
    /// "this daemon", not "this key".
    pub async fn get_total_rate(&self) -> Result<RateMetrics> {
        let start = window_start(TimeWindow::LastMinute).to_rfc3339();
        let rows: Vec<(i64, i64)> = requests::Entity::find()
            .select_only()
            .column(requests::Column::PromptTokens)
            .column(requests::Column::CompletionTokens)
            .filter(requests::Column::CreatedAt.gte(start))
            .into_tuple()
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("get_total_rate: {e}")))?;
        let tokens: u64 = rows
            .iter()
            .map(|(p, c)| (*p).max(0) as u64 + (*c).max(0) as u64)
            .sum();
        Ok(RateMetrics {
            requests_per_minute: rows.len() as f64,
            tokens_per_minute: tokens as f64,
        })
    }

    /// Spend + request count for one native ACP session under one **claimed**
    /// controller, within one API principal — the figure a controller may
    /// decorate onto the harness's own `UsageUpdate`.
    ///
    /// The key mirrors the route lease's `(api_principal, controller,
    /// session)`, and each third of it does different work. The principal is
    /// the only authorization boundary: it comes from the ordinary API
    /// credential, and one principal must not read another's spend even when
    /// both declare the same controller. The controller and session are
    /// *claims* — nothing verifies them — so they narrow the figure without
    /// vouching for it.
    ///
    /// Rows written before the principal was recorded carry null and match
    /// nothing. That is deliberate: their attribution is unknown, and folding
    /// them into whoever asks would be a guess presented as a measurement.
    ///
    /// Within the controller, the manager's opaque id is matched against
    /// every native carrier the identity hook persists — the trusted
    /// `acp_session_id` binding, the native root, and the native thread. A
    /// child agent shares its root's `native_root_session_id` and differs on
    /// `native_agent_thread_id` (§4.2), so a root's figure is the whole tree's;
    /// a Codex fork keeps the root `session-id` under a new `thread-id`, so it
    /// is reachable by its own id as well as counted under the root. Rows with
    /// no session identity never match — `NULL = ?` is not true on any backend.
    pub async fn spend_summary_for_acp_session(
        &self,
        api_principal: &str,
        controller_instance_id: &str,
        session_id: &str,
        window: TimeWindow,
    ) -> Result<SpendSummary> {
        let start = window_start(window).to_rfc3339();
        let mut query = requests::Entity::find()
            .select_only()
            .column(requests::Column::EstimatedChargeMicroUsd)
            .column(requests::Column::ChargeStatus)
            .filter(requests::Column::RouteScopeId.eq(api_principal))
            .filter(requests::Column::ControllerInstanceId.eq(controller_instance_id))
            .filter(
                Condition::any()
                    .add(requests::Column::AcpSessionId.eq(session_id))
                    .add(requests::Column::NativeRootSessionId.eq(session_id))
                    .add(requests::Column::NativeAgentThreadId.eq(session_id)),
            )
            .filter(requests::Column::CreatedAt.gte(start));
        // A custom window must honor its exclusive upper bound.
        // carries an exclusive end, and dropping it silently over-reports.
        if let TimeWindow::Custom { end, .. } = window {
            query = query.filter(requests::Column::CreatedAt.lt(end.to_rfc3339()));
        }
        let charges: Vec<(i64, String)> =
            query.into_tuple().all(&self.db).await.map_err(|e| {
                BitrouterError::internal(format!("spend_summary_for_acp_session: {e}"))
            })?;
        Ok(summarize(charges))
    }

    /// Total spend + request count within `window`, across every caller.
    pub async fn spend_summary(&self, window: TimeWindow) -> Result<SpendSummary> {
        self.spend_summary_filtered(window, None, None).await
    }

    /// Total spend + request count within the selected host-wide filters.
    ///
    /// This deliberately uses the same predicates as
    /// [`Self::recent_requests_filtered`], except for its page limit. A total
    /// computed before model/provider filtering, or after limiting rows, would
    /// describe a different set of requests from the page beside it.
    pub async fn spend_summary_filtered(
        &self,
        window: TimeWindow,
        model: Option<&str>,
        provider: Option<&str>,
    ) -> Result<SpendSummary> {
        let start = window_start(window).to_rfc3339();
        let mut query = requests::Entity::find()
            .select_only()
            .column(requests::Column::EstimatedChargeMicroUsd)
            .column(requests::Column::ChargeStatus)
            .filter(requests::Column::CreatedAt.gte(start));
        if let Some(model) = model {
            query = query.filter(requests::Column::ModelId.eq(model));
        }
        if let Some(provider) = provider {
            query = query.filter(requests::Column::ProviderId.eq(provider));
        }
        if let TimeWindow::Custom { end, .. } = window {
            query = query.filter(requests::Column::CreatedAt.lt(end.to_rfc3339()));
        }
        let charges: Vec<(i64, String)> = query
            .into_tuple()
            .all(&self.db)
            .await
            .map_err(|e| BitrouterError::internal(format!("spend_summary_filtered: {e}")))?;
        Ok(summarize(charges))
    }

    /// Record one settled request. The single writer.
    pub async fn record_request(&self, record: RequestMetric) -> Result<()> {
        let session_identity = record.session_identity.as_ref();
        let raw_usage_json = record
            .raw_usage
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| BitrouterError::internal(format!("serialize raw usage: {error}")))?;
        let charge_evidence_json = serde_json::to_string(&record.charge_evidence)
            .map(Some)
            .map_err(|error| {
                BitrouterError::internal(format!("serialize charge evidence: {error}"))
            })?;
        let row = requests::ActiveModel {
            request_id: Set(record.request_id),
            user_id: Set(record.user_id),
            api_key_id: Set(record.api_key_id),
            launch_id: Set(record.launch_id),
            route_scope_id: Set(session_identity.map(|identity| identity.route_scope_id.clone())),
            agent_harness: Set(session_identity.and_then(|identity| identity.agent_harness.clone())),
            controller_instance_id: Set(
                session_identity.and_then(|identity| identity.controller_instance_id.clone())
            ),
            acp_session_id: Set(
                session_identity.and_then(|identity| identity.acp_session_id.clone())
            ),
            native_root_session_id: Set(
                session_identity.and_then(|identity| identity.native_root_session_id.clone())
            ),
            native_agent_thread_id: Set(
                session_identity.and_then(|identity| identity.native_agent_thread_id.clone())
            ),
            native_parent_agent_thread_id: Set(session_identity
                .and_then(|identity| identity.native_parent_agent_thread_id.clone())),
            native_turn_id: Set(
                session_identity.and_then(|identity| identity.native_turn_id.clone())
            ),
            route_lease_id: Set(
                session_identity.and_then(|identity| identity.route_lease_id.clone())
            ),
            session_identity_json: Set(session_identity.map(|identity| identity.serialized.clone())),
            router_id: Set(record.router_id),
            binding_digest: Set(record.binding_digest),
            original_selector: Set(record.original_selector),
            model_id: Set(record.model_id),
            provider_id: Set(record.provider_id),
            prompt_tokens: Set(record.prompt_tokens as i64),
            completion_tokens: Set(record.completion_tokens as i64),
            reasoning_tokens: Set(record.reasoning_tokens as i64),
            cache_read_tokens: Set(record.cache_read_tokens as i64),
            cache_write_tokens: Set(record.cache_write_tokens as i64),
            uncached_input_tokens: Set(record.uncached_input_tokens as i64),
            output_tokens: Set(record.output_tokens as i64),
            usage_origin: Set(record.usage_origin.as_str().to_string()),
            raw_usage_json: Set(raw_usage_json),
            charge_status: Set(record.charge_status.as_str().to_string()),
            charge_evidence_json: Set(charge_evidence_json),
            reconciliation_status: Set(record.reconciliation_status.as_str().to_string()),
            reconciliation_attempts: Set(0),
            reconciliation_last_error: Set(None),
            reconciliation_last_attempt_at: Set(None),
            authoritative_settled_at: Set(None),
            authoritative_receipt_json: Set(None),
            estimated_charge_micro_usd: Set(record.estimated_charge_micro_usd),
            streamed: Set(record.streamed as i32),
            latency_ms: Set(record.latency_ms as i64),
            generation_time_ms: Set(record.generation_time_ms as i64),
            error: Set(record.error),
            created_at: Set(Utc::now().to_rfc3339()),
        };
        let mut charge = self.reserve_write(&row, 0)?;
        if let Some(charge) = &mut charge {
            charge.start();
        }
        match requests::Entity::insert(row.clone()).exec(&self.db).await {
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.sql_err(),
                    Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
                ) =>
            {
                // A delivery can refresh its mutable accounting fields. All
                // historical and current rows keep the original owner and
                // reconciliation evidence, including on a concurrent insert.
                let mut update = row;
                let id = update.request_id.take().expect("prepared request ID");
                let user = update.user_id.take().expect("prepared user ID");
                let key = update.api_key_id.take().expect("prepared API key ID");
                let scope = update.route_scope_id.take().expect("prepared route scope");
                let byte_eq =
                    |column: requests::Column, mysql_comparison: &'static str, value: String| {
                        if self.db.get_database_backend() == DbBackend::MySql {
                            sea_orm::sea_query::Expr::cust_with_values(mysql_comparison, [value])
                        } else {
                            column.eq(value)
                        }
                    };
                let scope = match scope {
                    Some(scope) => byte_eq(
                        requests::Column::RouteScopeId,
                        "BINARY route_scope_id = BINARY ?",
                        scope,
                    ),
                    None => requests::Column::RouteScopeId.is_null(),
                };
                let owner = Condition::all()
                    .add(byte_eq(
                        requests::Column::RequestId,
                        "BINARY request_id = BINARY ?",
                        id,
                    ))
                    .add(byte_eq(
                        requests::Column::UserId,
                        "BINARY user_id = BINARY ?",
                        user,
                    ))
                    .add(byte_eq(
                        requests::Column::ApiKeyId,
                        "BINARY api_key_id = BINARY ?",
                        key,
                    ))
                    .add(scope);
                update.created_at = sea_orm::NotSet;
                update.reconciliation_status = sea_orm::NotSet;
                update.reconciliation_attempts = sea_orm::NotSet;
                update.reconciliation_last_error = sea_orm::NotSet;
                update.reconciliation_last_attempt_at = sea_orm::NotSet;
                update.authoritative_settled_at = sea_orm::NotSet;
                update.authoritative_receipt_json = sea_orm::NotSet;
                let result = requests::Entity::update_many()
                    .set(update)
                    .filter(owner.clone())
                    .exec(&self.db)
                    .await
                    .map_err(|_| BitrouterError::internal("record_request refresh failed"))?;
                // MySQL may report zero changed rows for an identical refresh.
                if result.rows_affected == 0
                    && requests::Entity::find()
                        .filter(owner)
                        .count(&self.db)
                        .await
                        .map_err(|_| {
                            BitrouterError::internal("record_request ownership check failed")
                        })?
                        == 0
                {
                    storage::complete(charge)?;
                    return Err(BitrouterError::bad_request(
                        "request ID belongs to a different ledger owner",
                    ));
                }
            }
            Err(_) => return Err(BitrouterError::internal("record_request write failed")),
        }
        storage::complete(charge)?;
        Ok(())
    }
}

/// Resolve a [`TimeWindow`] into an inclusive lower-bound timestamp.
fn window_start(window: TimeWindow) -> DateTime<Utc> {
    let now = Utc::now();
    match window {
        TimeWindow::LastMinute => now - Duration::minutes(1),
        TimeWindow::Today => Utc
            .with_ymd_and_hms(now.year(), now.month(), now.day(), 0, 0, 0)
            .single()
            .unwrap_or(now),
        TimeWindow::ThisMonth => Utc
            .with_ymd_and_hms(now.year(), now.month(), 1, 0, 0, 0)
            .single()
            .unwrap_or(now),
        TimeWindow::Custom { start, .. } => start,
    }
}
