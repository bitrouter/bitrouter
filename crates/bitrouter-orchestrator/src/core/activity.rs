//! Monotonic active wall time: overlapping executions form one interval.
//! Idle approval, provider admission and checkpoint waits do not start activity.

use std::collections::BTreeSet;
use std::time::Instant;

/// Request-local gate callback time, including time awaiting another commit's
/// ACK. A guard records early returns as well as successful admission.
#[derive(Default)]
pub(crate) struct GateTime(std::sync::atomic::AtomicU64);

impl GateTime {
    pub fn reset(&self) {
        self.0.store(0, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::from_nanos(self.0.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn measure(&self) -> GateMeasurement<'_> {
        GateMeasurement {
            time: self,
            started: Instant::now(),
        }
    }
}

pub(crate) struct GateMeasurement<'a> {
    time: &'a GateTime,
    started: Instant,
}

impl Drop for GateMeasurement<'_> {
    fn drop(&mut self) {
        let nanos = self.started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
        let _ = self.time.0.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |prior| Some(prior.saturating_add(nanos)),
        );
    }
}

#[derive(Default)]
pub(crate) struct Activity {
    running: BTreeSet<String>,
    since: Option<Instant>,
    accumulated_ms: u64,
}

impl Activity {
    pub fn observe_elapsed(&mut self, elapsed_ms: u64) {
        self.accumulated_ms = self
            .accumulated_ms
            .saturating_add(elapsed_ms.saturating_sub(self.elapsed_ms()));
    }

    pub fn restored(accumulated_ms: u64) -> Self {
        Self {
            accumulated_ms,
            ..Self::default()
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restored_active_time_adds_new_work_without_charging_idle_time() {
        let mut activity = Activity::restored(5000);
        assert_eq!(activity.elapsed_ms(), 5000);
        activity.start("resumed".into());
        std::thread::sleep(std::time::Duration::from_millis(10));
        let settled = activity.finish("resumed");
        assert!(settled >= 5010);
        assert_eq!(activity.elapsed_ms(), settled);
    }
}
