//! Monotonic active wall time: overlapping executions form one interval.
//! Idle approval, provider admission and checkpoint waits do not start activity.

use std::collections::BTreeSet;
use std::time::Instant;

#[derive(Default)]
pub(crate) struct Activity {
    running: BTreeSet<String>,
    since: Option<Instant>,
    accumulated_ms: u64,
}

impl Activity {
    pub fn start(&mut self, id: String) {
        if self.running.is_empty() {
            self.since = Some(Instant::now());
        }
        self.running.insert(id);
    }

    pub fn finish(&mut self, id: &str) -> u64 {
        if self.running.remove(id) && self.running.is_empty() {
            self.accumulated_ms = self.elapsed_ms();
            self.since = None;
        }
        self.elapsed_ms()
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.accumulated_ms
            .saturating_add(self.since.map_or(0, |since| {
                since.elapsed().as_millis().min(u64::MAX as u128) as u64
            }))
    }
}
