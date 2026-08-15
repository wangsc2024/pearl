//! Time source.
//!
//! Time is injected rather than read from the ambient system so that lease expiry,
//! heartbeat staleness and retry backoff can be tested deterministically. A lease
//! reaper whose behaviour can only be observed by sleeping is a reaper nobody tests.

use chrono::{DateTime, TimeDelta, Utc};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

/// A source of the current instant.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Reads the real system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// A clock under test control.
///
/// Stores epoch milliseconds in an atomic so it can be advanced from another thread
/// while a component under test holds it.
#[derive(Debug, Clone)]
pub struct TestClock {
    millis: Arc<AtomicI64>,
}

impl TestClock {
    /// Starts at a fixed, readable instant: 2026-08-15T00:00:00Z.
    pub fn new() -> Self {
        Self::at(DateTime::from_timestamp(1_786_838_400, 0).expect("valid fixed timestamp"))
    }

    pub fn at(instant: DateTime<Utc>) -> Self {
        Self {
            millis: Arc::new(AtomicI64::new(instant.timestamp_millis())),
        }
    }

    /// Moves time forward.
    pub fn advance(&self, delta: TimeDelta) {
        self.millis
            .fetch_add(delta.num_milliseconds(), Ordering::SeqCst);
    }

    pub fn advance_secs(&self, secs: i64) {
        self.advance(TimeDelta::try_seconds(secs).expect("finite second count"));
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TestClock {
    fn now(&self) -> DateTime<Utc> {
        DateTime::from_timestamp_millis(self.millis.load(Ordering::SeqCst))
            .expect("test clock holds a valid timestamp")
    }
}

/// Shared handle to a clock.
pub type SharedClock = Arc<dyn Clock>;

/// A real-time shared clock.
pub fn system_clock() -> SharedClock {
    Arc::new(SystemClock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_moves_forward() {
        let clock = SystemClock;
        let first = clock.now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(clock.now() >= first);
    }

    #[test]
    fn test_clock_is_frozen_until_advanced() {
        let clock = TestClock::new();
        let first = clock.now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert_eq!(clock.now(), first, "test clock must not drift on its own");
    }

    #[test]
    fn advancing_moves_time_by_exactly_the_delta() {
        let clock = TestClock::new();
        let before = clock.now();
        clock.advance_secs(90);
        assert_eq!((clock.now() - before).num_seconds(), 90);
    }

    #[test]
    fn clones_share_the_same_timeline() {
        let clock = TestClock::new();
        let handle = clock.clone();
        clock.advance_secs(10);
        assert_eq!(handle.now(), clock.now(), "clone must observe the advance");
    }

    #[test]
    fn usable_as_a_trait_object() {
        let clock: SharedClock = Arc::new(TestClock::new());
        assert_eq!(clock.now(), TestClock::new().now());
    }
}
