/// A Unix-epoch timestamp in whole seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Returns the current time as a `Timestamp`.
    ///
    /// Uses `std::time::SystemTime` so it works without a Tokio runtime.
    pub fn now() -> Self {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is before UNIX epoch")
            .as_secs() as i64;
        Self(secs)
    }

    pub fn as_secs(&self) -> i64 {
        self.0
    }
}

impl From<i64> for Timestamp {
    fn from(secs: i64) -> Self {
        Self(secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_from_i64() {
        let ts = Timestamp::from(1_700_000_000);
        assert_eq!(ts.as_secs(), 1_700_000_000);
    }

    #[test]
    fn timestamp_now_is_positive() {
        assert!(Timestamp::now().as_secs() > 0);
    }

    #[test]
    fn timestamp_ordering() {
        let a = Timestamp::from(100);
        let b = Timestamp::from(200);
        assert!(a < b);
    }
}
