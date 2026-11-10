//! Config publication owns policy generations; this owner keeps one capacity pool.

use std::sync::{Arc, Mutex};

use super::{Scheduler, unavailable};
use crate::{
    config::{inference_routing::InferenceRoutingConfig, routing_table::ConfigRoutingTable},
    gateway::error::Result,
};

struct State {
    pool: Option<Arc<Scheduler>>,
    current: Option<(u64, Option<Arc<Scheduler>>)>,
    closed: bool,
}

pub(crate) struct Runtime {
    table: Option<Arc<ConfigRoutingTable>>,
    startup: InferenceRoutingConfig,
    state: Mutex<State>,
}

impl Runtime {
    pub(crate) fn new(table: Arc<ConfigRoutingTable>) -> Result<Arc<Self>> {
        let startup = table.snapshot_config().inference_routing;
        let runtime = Arc::new(Self {
            table: Some(table),
            startup,
            state: Mutex::new(State {
                pool: None,
                current: None,
                closed: false,
            }),
        });
        // Fail startup before serving if the configured pool cannot fit its budgets.
        runtime.snapshot()?;
        Ok(runtime)
    }

    #[cfg(test)]
    pub(crate) fn fixed(scheduler: Arc<Scheduler>) -> Arc<Self> {
        Arc::new(Self {
            table: None,
            startup: InferenceRoutingConfig::default(),
            state: Mutex::new(State {
                pool: Some(scheduler.clone()),
                current: Some((1, Some(scheduler))),
                closed: false,
            }),
        })
    }

    pub(crate) fn snapshot(&self) -> Result<Option<Arc<Scheduler>>> {
        // Never hold a routing-table lock with the pool lock. A request that
        // captured an older published generation may continue using it.
        let cached = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closed {
                return Err(unavailable());
            }
            state.current.clone()
        };
        let published = self.table.as_ref().and_then(|table| {
            table.inference_snapshot(
                cached
                    .as_ref()
                    .and_then(|(generation, _)| generation.checked_sub(1)),
            )
        });
        let Some((version, config)) = published else {
            return Ok(cached.and_then(|(_, snapshot)| snapshot));
        };
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err(unavailable());
        }
        let generation = version.checked_add(1).ok_or_else(unavailable)?;
        if let Some((current, snapshot)) = &state.current
            && *current == generation
        {
            return Ok(snapshot.clone());
        }
        if self.startup.restart_required(&config) {
            return Err(unavailable());
        }
        let snapshot = if config.enabled {
            Some(match &state.pool {
                Some(pool) => pool.snapshot(&config, generation),
                None => {
                    let pool = Scheduler::new(&config, generation)?;
                    state.pool = Some(pool.clone());
                    pool
                }
            })
        } else {
            None
        };
        if state
            .current
            .as_ref()
            .is_none_or(|(current, _)| *current < generation)
        {
            state.current = Some((generation, snapshot.clone()));
        }
        Ok(snapshot)
    }

    pub(crate) fn close(&self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        if let Some(pool) = &state.pool {
            pool.close();
        }
    }

    /// Preserve the legacy absent-scheduler drain contract until first activation.
    pub(crate) fn close_existing_pool(&self) -> Option<Arc<Scheduler>> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let pool = state.pool.clone()?;
        state.closed = true;
        pool.close();
        Some(pool)
    }

    pub(crate) async fn shutdown_existing_pool(&self) -> Result<()> {
        if let Some(pool) = self.close_existing_pool() {
            pool.shutdown().await?;
        }
        Ok(())
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.close();
        let pool = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .pool
            .clone();
        if let Some(pool) = pool {
            pool.shutdown().await?;
        }
        Ok(())
    }
}
