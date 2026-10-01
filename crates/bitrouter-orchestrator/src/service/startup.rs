//! Bounded cold discovery closes the gap between process startup and explicit
//! loading. Cold metadata does not grant access or become model context.
use super::*;
use crate::store::ExecutionHead;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StartupDiscovery {
    pub complete: bool,
    pub writer_fenced: bool,
    pub index_cutoff: u64,
    pub inspected_roots: usize,
    pub legacy_tasks: usize,
    pub blocked_workspaces: usize,
    pub scanned_records: u64,
    pub error: Option<ErrorCode>,
}

#[derive(Serialize)]
pub(super) struct ColdExecution {
    workspace: PathBuf,
    head: ExecutionHead,
    legacy: bool,
    requires_recovery: bool,
}
impl ColdExecution {
    pub(super) fn blocks(&self, workspace: &Path) -> bool {
        self.requires_recovery && self.workspace == workspace
    }
}

impl TaskService {
    pub(super) async fn discover_startup(&self, writer_fenced: bool) -> Result<(), ServiceError> {
        let result = self.scan_startup(writer_fenced).await;
        if let Err(error) = &result {
            let mut state = self.lock_state();
            let report = state
                .startup_discovery
                .get_or_insert_with(StartupDiscovery::default);
            report.complete = false;
            report.error = Some(error.code);
        }
        result
    }

    async fn scan_startup(&self, writer_fenced: bool) -> Result<(), ServiceError> {
        let mut report = StartupDiscovery {
            writer_fenced,
            ..Default::default()
        };
        self.lock_state().startup_discovery = Some(report.clone());
        let mut cold = HashMap::new();
        let mut metadata_bytes = 0_usize;
        let mut after = 0;
        let mut cutoff = None;
        loop {
            let page = self
                .inner
                .store
                .read_index(
                    after,
                    cutoff,
                    self.inner.limits.recovery_page_records,
                    self.inner.limits.recovery_page_bytes,
                )
                .await
                .map_err(storage)?;
            if cutoff.is_some_and(|cutoff| cutoff != page.cutoff)
                || page.entries.len() > self.inner.limits.recovery_page_records
                || page.entries.is_empty() && page.next_after.is_some()
            {
                return Err(storage("invalid startup index page"));
            }
            cutoff = Some(page.cutoff);
            report.index_cutoff = page.cutoff;
            for head in page.entries {
                if head.position <= after
                    || head.position > page.cutoff
                    || head.execution_id.is_empty()
                    || head.execution_id.len() > 128
                    || head.version == 0
                {
                    return Err(storage("invalid startup execution head"));
                }
                after = head.position;
                if cold.len() >= self.inner.limits.startup_roots {
                    return Err(ServiceError::new(
                        ErrorCode::Overloaded,
                        "startup execution root bound exceeded",
                    ));
                }
                if report.scanned_records >= self.inner.limits.startup_records {
                    return Err(ServiceError::new(
                        ErrorCode::Overloaded,
                        "startup record scan bound exceeded",
                    ));
                }
                let first = self
                    .inner
                    .store
                    .read_records(
                        &head.execution_id,
                        0,
                        Some(head.version),
                        1,
                        self.inner.limits.recovery_page_bytes,
                    )
                    .await
                    .map_err(storage)?
                    .ok_or("startup execution root disappeared")?;
                if first.cutoff != head.version
                    || first.records.len() != 1
                    || first.next_after != (head.version > 1).then_some(1)
                {
                    return Err(storage("invalid startup execution header page"));
                }
                let mut audit = recovery::StartupAudit::new(
                    first.records.first().ok_or("missing startup header")?,
                    &head.execution_id,
                    self.inner.limits.clone(),
                )?;
                let workspace = audit.workspace().to_path_buf();
                if !workspace.is_absolute() || workspace.to_str().is_none() {
                    return Err(storage("invalid stored startup workspace identity"));
                }
                let legacy = audit.legacy();
                let mut cursor = 1;
                report.scanned_records = report
                    .scanned_records
                    .checked_add(1)
                    .ok_or("startup record counter exhausted")?;
                audit.consume(first.records)?;
                // Without a writer fence heads may move. Enumerate the headers
                // for visibility, but never classify such a workspace as clean.
                while writer_fenced && cursor < head.version {
                    let remaining = self
                        .inner
                        .limits
                        .startup_records
                        .saturating_sub(report.scanned_records);
                    if remaining == 0 {
                        return Err(ServiceError::new(
                            ErrorCode::Overloaded,
                            "startup record scan bound exceeded",
                        ));
                    }
                    let page_limit = self
                        .inner
                        .limits
                        .recovery_page_records
                        .min(usize::try_from(remaining).unwrap_or(usize::MAX));
                    let records = self
                        .inner
                        .store
                        .read_records(
                            &head.execution_id,
                            cursor,
                            Some(head.version),
                            page_limit,
                            self.inner.limits.recovery_page_bytes,
                        )
                        .await
                        .map_err(storage)?
                        .ok_or("startup execution disappeared")?;
                    if records.cutoff != head.version || records.records.is_empty() {
                        return Err(storage("startup scan made no progress"));
                    }
                    cursor = cursor
                        .checked_add(records.records.len() as u64)
                        .ok_or("startup cursor exhausted")?;
                    report.scanned_records = report
                        .scanned_records
                        .checked_add(records.records.len() as u64)
                        .ok_or("startup record counter exhausted")?;
                    if cursor > head.version
                        || records.next_after != (cursor < head.version).then_some(cursor)
                    {
                        return Err(storage("invalid startup record cursor"));
                    }
                    if report.scanned_records > self.inner.limits.startup_records {
                        return Err(ServiceError::new(
                            ErrorCode::Overloaded,
                            "startup record scan bound exceeded",
                        ));
                    }
                    audit.consume(records.records)?;
                    self.lock_state().startup_discovery = Some(report.clone());
                    tokio::task::yield_now().await;
                }
                if report.scanned_records > self.inner.limits.startup_records {
                    return Err(ServiceError::new(
                        ErrorCode::Overloaded,
                        "startup record scan bound exceeded",
                    ));
                }
                let owner = self
                    .inner
                    .store
                    .read_owner(audit.epoch())
                    .await
                    .map_err(storage)?;
                let entry = ColdExecution {
                    workspace,
                    legacy,
                    requires_recovery: !writer_fenced || !audit.known_clean(owner.as_ref()),
                    head,
                };
                metadata_bytes = metadata_bytes
                    .saturating_add(serde_json::to_vec(&entry).map_err(storage)?.len());
                if metadata_bytes > self.inner.limits.startup_metadata_bytes {
                    return Err(ServiceError::new(
                        ErrorCode::Overloaded,
                        "startup metadata byte bound exceeded",
                    ));
                }
                report.legacy_tasks += usize::from(legacy);
                report.inspected_roots += 1;
                if cold
                    .insert(entry.head.execution_id.clone(), entry)
                    .is_some()
                {
                    return Err(storage("duplicate startup execution root"));
                }
                self.lock_state().startup_discovery = Some(report.clone());
            }
            match page.next_after {
                Some(next) if next == after => {}
                Some(_) => return Err(storage("invalid startup index cursor")),
                None => break,
            }
        }
        report.blocked_workspaces = cold
            .values()
            .filter(|entry| entry.requires_recovery)
            .map(|entry| &entry.workspace)
            .collect::<std::collections::HashSet<_>>()
            .len();
        report.complete = true;
        let mut state = self.lock_state();
        state.cold_executions = cold;
        state.startup_discovery = Some(report);
        Ok(())
    }
}
fn storage(error: impl ToString) -> ServiceError {
    ServiceError::new(ErrorCode::StorageUnavailable, error.to_string())
}
