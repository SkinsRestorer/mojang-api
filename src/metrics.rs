use std::{
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
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
    last_report_at: Mutex<Instant>,
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
            last_report_at: Mutex::new(now),
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

    pub fn snapshot_and_reset(&self) -> MetricsSnapshot {
        let now = Instant::now();
        let mut last_report_at = self
            .last_report_at
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let report_period = now.saturating_duration_since(*last_report_at);
        *last_report_at = now;

        MetricsSnapshot {
            uuid_requests: self.uuid_requests.swap(0, Ordering::Relaxed),
            skin_requests: self.skin_requests.swap(0, Ordering::Relaxed),
            uuid_cache_hits: self.uuid_cache_hits.swap(0, Ordering::Relaxed),
            uuid_cache_misses: self.uuid_cache_misses.swap(0, Ordering::Relaxed),
            skin_cache_hits: self.skin_cache_hits.swap(0, Ordering::Relaxed),
            skin_cache_misses: self.skin_cache_misses.swap(0, Ordering::Relaxed),
            batches_processed: self.batches_processed.swap(0, Ordering::Relaxed),
            usernames_batched: self.usernames_batched.swap(0, Ordering::Relaxed),
            bytes_sent_to_mojang: self.bytes_sent_to_mojang.swap(0, Ordering::Relaxed),
            bytes_received_from_mojang: self.bytes_received_from_mojang.swap(0, Ordering::Relaxed),
            mojang_requests: self.mojang_requests.swap(0, Ordering::Relaxed),
            mojang_errors: self.mojang_errors.swap(0, Ordering::Relaxed),
            uptime: now.saturating_duration_since(self.started_at),
            report_period,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
