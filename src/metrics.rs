use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct Metrics {
    uuid_requests: AtomicU64,
    skin_requests: AtomicU64,
    uuid_cache_hits: AtomicU64,
    uuid_cache_misses: AtomicU64,
    skin_cache_hits: AtomicU64,
    skin_cache_misses: AtomicU64,
    batches_processed: AtomicU64,
    usernames_batched: AtomicU64,
    bytes_sent_to_mojang: AtomicU64,
    bytes_received_from_mojang: AtomicU64,
    mojang_requests: AtomicU64,
    mojang_errors: AtomicU64,
    started_at: Instant,
}

impl Default for Metrics {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            uuid_requests: AtomicU64::new(0),
            skin_requests: AtomicU64::new(0),
            uuid_cache_hits: AtomicU64::new(0),
            uuid_cache_misses: AtomicU64::new(0),
            skin_cache_hits: AtomicU64::new(0),
            skin_cache_misses: AtomicU64::new(0),
            batches_processed: AtomicU64::new(0),
            usernames_batched: AtomicU64::new(0),
            bytes_sent_to_mojang: AtomicU64::new(0),
            bytes_received_from_mojang: AtomicU64::new(0),
            mojang_requests: AtomicU64::new(0),
            mojang_errors: AtomicU64::new(0),
            started_at: now,
        }
    }
}

impl Metrics {
    pub fn increment_uuid_requests(&self) {
        self.uuid_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_skin_requests(&self) {
        self.skin_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_uuid_cache_hits(&self) {
        self.uuid_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_uuid_cache_misses(&self) {
        self.uuid_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_skin_cache_hits(&self) {
        self.skin_cache_hits.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_skin_cache_misses(&self) {
        self.skin_cache_misses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_batch(&self, usernames: usize) {
        self.batches_processed.fetch_add(1, Ordering::Relaxed);
        self.usernames_batched
            .fetch_add(usernames as u64, Ordering::Relaxed);
    }

    pub fn record_mojang_request(&self, sent_bytes: usize) {
        self.mojang_requests.fetch_add(1, Ordering::Relaxed);
        self.bytes_sent_to_mojang
            .fetch_add(sent_bytes as u64, Ordering::Relaxed);
    }

    pub fn record_mojang_response(&self, received_bytes: usize) {
        self.bytes_received_from_mojang
            .fetch_add(received_bytes as u64, Ordering::Relaxed);
    }

    pub fn increment_mojang_errors(&self) {
        self.mojang_errors.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        let now = Instant::now();
        MetricsSnapshot {
            uuid_requests: self.uuid_requests.load(Ordering::Relaxed),
            skin_requests: self.skin_requests.load(Ordering::Relaxed),
            uuid_cache_hits: self.uuid_cache_hits.load(Ordering::Relaxed),
            uuid_cache_misses: self.uuid_cache_misses.load(Ordering::Relaxed),
            skin_cache_hits: self.skin_cache_hits.load(Ordering::Relaxed),
            skin_cache_misses: self.skin_cache_misses.load(Ordering::Relaxed),
            batches_processed: self.batches_processed.load(Ordering::Relaxed),
            usernames_batched: self.usernames_batched.load(Ordering::Relaxed),
            bytes_sent_to_mojang: self.bytes_sent_to_mojang.load(Ordering::Relaxed),
            bytes_received_from_mojang: self.bytes_received_from_mojang.load(Ordering::Relaxed),
            mojang_requests: self.mojang_requests.load(Ordering::Relaxed),
            mojang_errors: self.mojang_errors.load(Ordering::Relaxed),
            uptime: now.saturating_duration_since(self.started_at),
            report_period: Duration::ZERO,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub uuid_requests: u64,
    pub skin_requests: u64,
    pub uuid_cache_hits: u64,
    pub uuid_cache_misses: u64,
    pub skin_cache_hits: u64,
    pub skin_cache_misses: u64,
    pub batches_processed: u64,
    pub usernames_batched: u64,
    pub bytes_sent_to_mojang: u64,
    pub bytes_received_from_mojang: u64,
    pub mojang_requests: u64,
    pub mojang_errors: u64,
    pub uptime: Duration,
    pub report_period: Duration,
}

impl MetricsSnapshot {
    #[must_use]
    pub fn since(self, previous: Self, report_period: Duration) -> Self {
        Self {
            uuid_requests: self.uuid_requests.saturating_sub(previous.uuid_requests),
            skin_requests: self.skin_requests.saturating_sub(previous.skin_requests),
            uuid_cache_hits: self
                .uuid_cache_hits
                .saturating_sub(previous.uuid_cache_hits),
            uuid_cache_misses: self
                .uuid_cache_misses
                .saturating_sub(previous.uuid_cache_misses),
            skin_cache_hits: self
                .skin_cache_hits
                .saturating_sub(previous.skin_cache_hits),
            skin_cache_misses: self
                .skin_cache_misses
                .saturating_sub(previous.skin_cache_misses),
            batches_processed: self
                .batches_processed
                .saturating_sub(previous.batches_processed),
            usernames_batched: self
                .usernames_batched
                .saturating_sub(previous.usernames_batched),
            bytes_sent_to_mojang: self
                .bytes_sent_to_mojang
                .saturating_sub(previous.bytes_sent_to_mojang),
            bytes_received_from_mojang: self
                .bytes_received_from_mojang
                .saturating_sub(previous.bytes_received_from_mojang),
            mojang_requests: self
                .mojang_requests
                .saturating_sub(previous.mojang_requests),
            mojang_errors: self.mojang_errors.saturating_sub(previous.mojang_errors),
            uptime: self.uptime,
            report_period,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Metrics;

    #[test]
    fn derives_report_windows_without_resetting_counters() {
        let metrics = Metrics::default();
        let baseline = metrics.snapshot();
        metrics.increment_uuid_requests();
        metrics.increment_uuid_cache_misses();
        metrics.record_mojang_request(12);
        metrics.record_mojang_response(34);

        let current = metrics.snapshot();
        let window = current.since(baseline, Duration::from_mins(5));

        assert_eq!(window.uuid_requests, 1);
        assert_eq!(window.uuid_cache_misses, 1);
        assert_eq!(window.mojang_requests, 1);
        assert_eq!(window.bytes_sent_to_mojang, 12);
        assert_eq!(window.bytes_received_from_mojang, 34);
        assert_eq!(window.report_period, Duration::from_mins(5));
        assert_eq!(metrics.snapshot().uuid_requests, 1);
    }
}
