//! Admission and execution have different clocks; neither renews a total limit.
use std::time::Duration;
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Deadline {
    Submission(Instant),
    Execution(Option<Instant>),
}

impl Deadline {
    pub(super) fn admit(self, total: Option<u64>) -> Self {
        match self {
            Self::Submission(_) => {
                Self::Execution(total.map(|seconds| after(Duration::from_secs(seconds))))
            }
            // A child keeps its parent's ceiling; admission cannot renew it.
            Self::Execution(_) => self,
        }
    }

    pub(crate) fn instant(self) -> Option<Instant> {
        match self {
            Self::Submission(deadline) => Some(deadline),
            Self::Execution(deadline) => deadline,
        }
    }

    pub(crate) fn expired(self) -> bool {
        self.instant()
            .is_some_and(|deadline| deadline <= Instant::now())
    }

    pub(crate) fn min(self, bound: Instant) -> Instant {
        self.instant().map_or(bound, |deadline| deadline.min(bound))
    }

    pub(crate) async fn elapsed(self) {
        match self.instant() {
            Some(deadline) => tokio::time::sleep_until(deadline).await,
            None => std::future::pending().await,
        }
    }
}

pub(super) fn after(duration: Duration) -> Instant {
    let now = Instant::now();
    // Configuration is validated at startup. A host clock overflow still fails
    // closed, never panics or silently disables the requested limit.
    now.checked_add(duration).unwrap_or(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn admission_expiry_does_not_truncate_execution_and_children_cannot_renew_total() {
        let start = Instant::now();
        let submission = Deadline::Submission(start + Duration::from_secs(60));
        let unlimited = submission.admit(None);
        let bounded = submission.admit(Some(180));
        tokio::time::advance(Duration::from_secs(75)).await;
        assert!(submission.expired());
        assert!(!unlimited.expired());
        assert!(!bounded.expired());
        assert_eq!(bounded.admit(Some(180)), bounded);
        assert_eq!(unlimited.admit(Some(180)), unlimited);
        assert_eq!(
            unlimited.min(Instant::now() + Duration::from_secs(5)),
            Instant::now() + Duration::from_secs(5)
        );
        tokio::time::advance(Duration::from_secs(105)).await;
        assert!(bounded.expired());
        assert!(!unlimited.expired());
        assert!(futures::FutureExt::now_or_never(bounded.elapsed()).is_some());
        assert!(futures::FutureExt::now_or_never(unlimited.elapsed()).is_none());
    }
}
