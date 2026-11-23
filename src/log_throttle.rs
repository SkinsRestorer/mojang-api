use std::{
    sync::{Mutex, PoisonError},
    time::Duration,
};

use tokio::time::Instant;

/// How often a repeated warning may be logged.
pub const LOG_INTERVAL: Duration = Duration::from_mins(1);

/// Limits a repeated log event to one line per interval.
///
/// The first event is logged. Later events in the same interval are counted, and the next
/// logged line reports how many were suppressed. A count is only reported when another
/// event arrives, so a burst that ends inside an interval does not report its tail.
#[derive(Debug)]
pub struct LogThrottle {
    interval: Duration,
    state: Mutex<State>,
}

#[derive(Debug)]
struct State {
    next_log_at: Option<Instant>,
    suppressed: u64,
}

impl LogThrottle {
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            state: Mutex::new(State {
                next_log_at: None,
                suppressed: 0,
            }),
        }
    }

    /// Records an event. Returns the number of events suppressed since the last logged one
    /// when this event should be logged, or `None` when it should be suppressed.
    pub fn check(&self) -> Option<u64> {
        let now = Instant::now();
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.next_log_at.is_some_and(|next| now < next) {
            state.suppressed = state.suppressed.saturating_add(1);
            return None;
        }
        state.next_log_at = now.checked_add(self.interval);
        Some(std::mem::take(&mut state.suppressed))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::LogThrottle;

    #[tokio::test(start_paused = true)]
    async fn logs_once_per_interval_with_the_suppressed_count() {
        let throttle = LogThrottle::new(Duration::from_secs(60));
        assert_eq!(
            throttle.check(),
            Some(0),
            "the first event should be logged"
        );
        assert_eq!(throttle.check(), None, "a repeat should be suppressed");
        tokio::time::advance(Duration::from_secs(59)).await;
        assert_eq!(throttle.check(), None, "the interval has not passed yet");

        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(
            throttle.check(),
            Some(2),
            "the next line should report the suppressed events"
        );
        assert_eq!(throttle.check(), None, "a new interval should start");

        tokio::time::advance(Duration::from_secs(120)).await;
        assert_eq!(
            throttle.check(),
            Some(1),
            "a quiet period should not hide the earlier count"
        );
    }
}
