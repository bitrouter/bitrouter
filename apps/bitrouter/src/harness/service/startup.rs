//! Bounded cold discovery closes the gap between process startup and explicit
//! loading. Cold metadata does not grant access or become model context.

use bitrouter_ai::harness::runtime::StartupDiscovery;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::ServiceError;
use super::ThreadService;
use super::recovery;
use bitrouter_ai::harness::runtime::ErrorCode;

pub(super) struct ColdExecution {
    workspace: PathBuf,
}
impl ColdExecution {
    pub(super) fn blocks(&self, workspace: &Path) -> bool {
        self.workspace == workspace
    }

    fn retained_bytes(&self, key_capacity: usize) -> Result<usize, ServiceError> {
        crate::config::startup::UNRESOLVED_ENTRY_OVERHEAD
            .checked_add(key_capacity)
            .and_then(|bytes| bytes.checked_add(self.workspace.capacity()))
            .ok_or_else(|| storage("startup metadata accounting overflow"))
    }
}

impl ThreadService {
    pub(super) async fn discover_startup(&self, writer_fenced: bool) -> Result<(), ServiceError> {
        let result = self.scan_startup(writer_fenced).await;
        if let Err(error) = &result {
            let mut state = self.lock_state();
            let report = state
                .startup_discovery
                .get_or_insert_with(StartupDiscovery::default);
            if error.message.contains("unsupported_runtime_format") {
                self.inner
                    .cleanup_unconfirmed
                    .store(true, std::sync::atomic::Ordering::Release);
            }
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
        let mut remaining = self.inner.limits.startup_records;
        let mut after = 0;
        let mut cutoff = None;
        loop {
            self.check_startup_open()?;
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
                self.check_startup_open()?;
                crate::harness::storage::validate_runtime_format(head.format_version)
                    .map_err(storage)?;
                if head.position <= after
                    || head.position > page.cutoff
                    || head.execution_id.is_empty()
                    || head.execution_id.len() > 128
                    || head.version == 0
                {
                    return Err(storage("invalid startup execution head"));
                }
                after = head.position;
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
                let mut cursor = 1;
                report.scanned_records = report
                    .scanned_records
                    .checked_add(1)
                    .ok_or("startup record counter exhausted")?;
                audit.consume(first.records)?;
                remaining -= 1;
                if remaining == 0 {
                    self.startup_progress(&report).await?;
                    remaining = self.inner.limits.startup_records;
                }
                // Without a writer fence heads may move. Enumerate the headers
                // for visibility, but never classify such a workspace as clean.
                while writer_fenced && cursor < head.version {
                    self.check_startup_open()?;
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
                    if records.cutoff != head.version
                        || records.records.is_empty()
                        || records.records.len() > page_limit
                    {
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
                    remaining -= records.records.len() as u64;
                    audit.consume(records.records)?;
                    self.startup_progress(&report).await?;
                    if remaining == 0 {
                        remaining = self.inner.limits.startup_records;
                    }
                }
                self.check_startup_open()?;
                let owner = self
                    .inner
                    .store
                    .read_owner(audit.epoch())
                    .await
                    .map_err(storage)?;
                report.inspected_roots = report
                    .inspected_roots
                    .checked_add(1)
                    .ok_or("startup root counter exhausted")?;
                if !writer_fenced || !audit.known_clean(owner.as_ref()) {
                    let entry = ColdExecution { workspace };
                    if cold.len() >= self.inner.limits.startup_roots {
                        return Err(ServiceError::new(
                            ErrorCode::Overloaded,
                            "startup unresolved root budget exceeded",
                        ));
                    }
                    metadata_bytes = metadata_bytes
                        .checked_add(entry.retained_bytes(head.execution_id.capacity())?)
                        .ok_or("startup metadata accounting overflow")?;
                    if metadata_bytes > self.inner.limits.startup_metadata_bytes {
                        return Err(ServiceError::new(
                            ErrorCode::Overloaded,
                            "startup unresolved metadata budget exceeded",
                        ));
                    }
                    cold.try_reserve(1).map_err(|_| {
                        ServiceError::new(
                            ErrorCode::Overloaded,
                            "startup unresolved metadata allocation failed",
                        )
                    })?;
                    if cold.insert(head.execution_id, entry).is_some() {
                        return Err(storage("duplicate startup execution root"));
                    }
                }
                self.startup_progress(&report).await?;
            }
            match page.next_after {
                Some(next) if next == after => {}
                Some(_) => return Err(storage("invalid startup index cursor")),
                None => break,
            }
        }
        report.blocked_workspaces = cold
            .values()
            .map(|entry| &entry.workspace)
            .collect::<std::collections::HashSet<_>>()
            .len();
        let mut state = self.lock_state();
        if state.closing {
            return Err(startup_closed());
        }
        report.complete = true;
        state.cold_executions = cold;
        state.startup_discovery = Some(report);
        Ok(())
    }

    fn check_startup_open(&self) -> Result<(), ServiceError> {
        if self.lock_state().closing {
            return Err(startup_closed());
        }
        Ok(())
    }

    async fn startup_progress(&self, report: &StartupDiscovery) -> Result<(), ServiceError> {
        {
            let mut state = self.lock_state();
            if state.closing {
                return Err(startup_closed());
            }
            state.startup_discovery = Some(report.clone());
        }
        tokio::task::yield_now().await;
        self.check_startup_open()
    }
}

fn startup_closed() -> ServiceError {
    ServiceError::new(
        ErrorCode::ShuttingDown,
        "runtime is shutting down during startup discovery",
    )
}
fn storage(error: impl ToString) -> ServiceError {
    ServiceError::storage(error)
}
