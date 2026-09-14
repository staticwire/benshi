//! Time, taken as a dependency rather than read from the environment.
//!
//! Every component that needs the current moment receives a [`Clock`]. A test
//! then advances twenty seconds at no wall-clock cost, which is the difference
//! between a timeline test suite that runs in milliseconds and one nobody runs.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// A monotonic instant, measured from an unspecified epoch.
///
/// Unlike [`std::time::Instant`] this is an ordinary value: it serialises, so a
/// recorded trace replays to exactly the state that was recorded. The epoch is
/// arbitrary and only differences between timestamps are meaningful; comparing
/// timestamps taken from two different [`Clock`] instances is not defined.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Timestamp(Duration);

impl Timestamp {
    /// How much time separates this timestamp from an earlier one.
    ///
    /// Saturates at zero when `earlier` is in fact later, because a monotonic
    /// clock running backwards is a bug in the clock rather than a negative
    /// duration every caller should have to handle.
    #[must_use]
    pub fn since(self, earlier: Self) -> Duration {
        self.0.saturating_sub(earlier.0)
    }
}

/// A source of the current moment.
///
/// Implementations must be monotonic: `now()` never returns a value smaller
/// than one it has already returned.
pub trait Clock: Send + Sync {
    /// The current moment.
    fn now(&self) -> Timestamp;
}

/// The real clock, backed by [`std::time::Instant`].
#[derive(Debug)]
pub struct SystemClock {
    origin: std::time::Instant,
}

impl SystemClock {
    /// A clock whose epoch is the moment it was created.
    #[must_use]
    pub fn new() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp(self.origin.elapsed())
    }
}

/// A clock that moves only when a test moves it.
#[derive(Debug)]
pub struct TestClock {
    elapsed: Mutex<Duration>,
}

impl TestClock {
    /// A clock stopped at its epoch.
    #[must_use]
    pub fn new() -> Self {
        Self {
            elapsed: Mutex::new(Duration::ZERO),
        }
    }

    /// Move the clock forward.
    pub fn advance(&self, delta: Duration) {
        *self.lock() += delta;
    }

    /// The elapsed duration, behind its lock.
    ///
    /// A poisoned lock is recovered from rather than propagated: the duration
    /// behind it is still a valid duration, and a panic here would report the
    /// unrelated failure that poisoned the lock.
    fn lock(&self) -> MutexGuard<'_, Duration> {
        self.elapsed.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for TestClock {
    fn now(&self) -> Timestamp {
        Timestamp(*self.lock())
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, TestClock, Timestamp};
    use std::time::Duration;

    #[test]
    fn a_test_clock_advances_only_when_told() {
        let clock = TestClock::new();
        let first = clock.now();
        clock.advance(Duration::from_secs(20));
        let second = clock.now();

        assert_eq!(second.since(first), Duration::from_secs(20));
    }

    #[test]
    fn twenty_seconds_of_pause_cost_no_wall_clock_time() {
        let started = std::time::Instant::now();
        let clock = TestClock::new();
        clock.advance(Duration::from_secs(20));
        assert!(started.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn a_timestamp_round_trips_through_json_exactly() {
        let clock = TestClock::new();
        clock.advance(Duration::from_micros(1_234_567));
        let stamp = clock.now();

        let text = serde_json::to_string(&stamp).expect("serialise");
        let back: Timestamp = serde_json::from_str(&text).expect("deserialise");

        assert_eq!(stamp, back);
    }
}
