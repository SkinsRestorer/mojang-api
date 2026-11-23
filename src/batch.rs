use std::{
    collections::HashMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    sync::{
        Semaphore,
        mpsc::{self, error::TrySendError},
        oneshot,
    },
    task::{JoinHandle, JoinSet},
    time::{MissedTickBehavior, interval, timeout},
};
use tracing::{error, warn};
use uuid::Uuid;

use crate::{
    cache::{CacheLoad, CacheManager},
    log_throttle::{LOG_INTERVAL, LogThrottle},
    metrics::Metrics,
    mojang::{MojangService, UPSTREAM_BUDGET, UpstreamError},
    rate_limit::ClientKey,
};

const BATCH_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// Mojang's bulk profile endpoints accept at most 10 names per request.
const MAX_BATCH_SIZE: usize = 10;
/// Flushes a partial batch after this long. Each tick sends at most one partial bulk request,
/// which stays near Mojang's sustained rate limit, while a cold lookup waits half a second on
/// average.
const BATCH_INTERVAL: Duration = Duration::from_secs(1);
/// Bounds how long a request waits for its batch, including time spent queued. It covers one
/// batch interval plus the Mojang budget, with a second of slack for busy batch slots, and stays
/// below the router deadline in `app.rs`.
pub(crate) const LOOKUP_TIMEOUT: Duration = Duration::from_secs(7);
const _: () = assert!(
    BATCH_INTERVAL.as_millis() + UPSTREAM_BUDGET.as_millis() < LOOKUP_TIMEOUT.as_millis(),
    "a lookup must outlast its batch interval and the Mojang call"
);
/// Bounds how many queued or in-flight lookups one client can hold, so clients that each stay
/// under the rate limit cannot fill the shared queue together.
const MAX_LOOKUPS_PER_CLIENT: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct BatchConfig {
    size: NonZeroUsize,
    interval: Duration,
    queue_capacity: NonZeroUsize,
    max_in_flight_batches: NonZeroUsize,
}

impl Default for BatchConfig {
    /// Flushes full batches of 10 names, or any pending names every second.
    fn default() -> Self {
        Self {
            size: NonZeroUsize::new(MAX_BATCH_SIZE).unwrap_or(NonZeroUsize::MIN),
            interval: BATCH_INTERVAL,
            queue_capacity: NonZeroUsize::new(1_024).unwrap_or(NonZeroUsize::MIN),
            max_in_flight_batches: NonZeroUsize::new(16).unwrap_or(NonZeroUsize::MIN),
        }
    }
}

impl BatchConfig {
    /// Creates batching limits.
    ///
    /// # Errors
    ///
    /// Returns an error when any limit or the batching interval is zero, or when the batch size
    /// exceeds [`MAX_BATCH_SIZE`].
    pub fn new(
        size: usize,
        interval: Duration,
        queue_capacity: usize,
        max_in_flight_batches: usize,
    ) -> Result<Self, BatchConfigError> {
        let size = NonZeroUsize::new(size).ok_or(BatchConfigError::ZeroSize)?;
        if size.get() > MAX_BATCH_SIZE {
            return Err(BatchConfigError::SizeTooLarge);
        }
        if interval.is_zero() {
            return Err(BatchConfigError::ZeroInterval);
        }
        let queue_capacity =
            NonZeroUsize::new(queue_capacity).ok_or(BatchConfigError::ZeroQueueCapacity)?;
        let max_in_flight_batches = NonZeroUsize::new(max_in_flight_batches)
            .ok_or(BatchConfigError::ZeroMaxInFlightBatches)?;

        Ok(Self {
            size,
            interval,
            queue_capacity,
            max_in_flight_batches,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum BatchConfigError {
    #[error("batch size must be greater than zero")]
    ZeroSize,
    #[error("batch size must not exceed {MAX_BATCH_SIZE}")]
    SizeTooLarge,
    #[error("batch interval must be greater than zero")]
    ZeroInterval,
    #[error("queue capacity must be greater than zero")]
    ZeroQueueCapacity,
    #[error("maximum in-flight batches must be greater than zero")]
    ZeroMaxInFlightBatches,
}

struct PendingLookup {
    name: String,
    response: oneshot::Sender<Result<Option<Uuid>, UpstreamError>>,
    /// Released once the batch answers the lookup or drops it unprocessed.
    _slot: ClientSlot,
}

/// Counts the lookups each client holds in the queue or in a running batch.
#[derive(Debug, Default)]
struct ClientLookups(Mutex<HashMap<ClientKey, usize>>);

impl ClientLookups {
    fn reserve(self: &Arc<Self>, client: ClientKey) -> Option<ClientSlot> {
        let mut counts = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = counts.entry(client).or_default();
        if *count >= MAX_LOOKUPS_PER_CLIENT {
            return None;
        }
        *count += 1;
        drop(counts);
        Some(ClientSlot {
            lookups: Arc::clone(self),
            client,
        })
    }

    #[cfg(test)]
    fn held_by(&self, client: ClientKey) -> usize {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&client)
            .copied()
            .unwrap_or_default()
    }
}

struct ClientSlot {
    lookups: Arc<ClientLookups>,
    client: ClientKey,
}

impl Drop for ClientSlot {
    fn drop(&mut self) {
        let mut counts = self
            .lookups
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = counts.get_mut(&self.client) {
            *count -= 1;
            // Removing idle clients keeps the map as small as the queue.
            if *count == 0 {
                counts.remove(&self.client);
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct BatchLookup {
    sender: mpsc::Sender<PendingLookup>,
    cache: CacheManager,
    metrics: Arc<Metrics>,
    client_lookups: Arc<ClientLookups>,
}

impl BatchLookup {
    /// Resolves a username from the cache or the next Mojang batch.
    ///
    /// # Errors
    ///
    /// Returns an upstream error when the queue is full or closed, when `client` already holds
    /// its share of the queue, when the lookup does not finish in time, or when Mojang cannot
    /// resolve the batch.
    pub async fn lookup(
        &self,
        mut name: String,
        client: ClientKey,
    ) -> Result<Option<Uuid>, UpstreamError> {
        name.make_ascii_lowercase();
        let request_name = name.clone();
        let sender = self.sender.clone();
        let metrics = Arc::clone(&self.metrics);
        let client_lookups = Arc::clone(&self.client_lookups);
        let result = self
            .cache
            .get_or_try_insert_uuid(name, async move {
                let slot = client_lookups.reserve(client).ok_or_else(|| {
                    warn!("rejecting Mojang lookup because the client holds too many lookups");
                    UpstreamError::Overloaded
                })?;
                let (response, receiver) = oneshot::channel();
                sender
                    .try_send(PendingLookup {
                        name: request_name,
                        response,
                        _slot: slot,
                    })
                    .map_err(|error| match error {
                        TrySendError::Full(_) => {
                            static QUEUE_FULL: LogThrottle = LogThrottle::new(LOG_INTERVAL);
                            metrics.increment_batch_queue_rejections();
                            if let Some(suppressed) = QUEUE_FULL.check() {
                                warn!(
                                    suppressed,
                                    "rejecting Mojang lookup because the batch queue is full"
                                );
                            }
                            UpstreamError::Overloaded
                        }
                        TrySendError::Closed(_) => {
                            warn!("could not queue Mojang lookup because batching has stopped");
                            UpstreamError::BatchStopped
                        }
                    })?;
                match timeout(LOOKUP_TIMEOUT, receiver).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(error)) => {
                        warn!(%error, "Mojang lookup ended without a response");
                        Err(UpstreamError::BatchStopped)
                    }
                    Err(_) => {
                        warn!("Mojang lookup did not finish before the deadline");
                        Err(UpstreamError::Timeout)
                    }
                }
            })
            .await
            .map_err(|error| *error);

        // A failed load was still a miss, so count it before returning the error.
        if result.as_ref().is_ok_and(CacheLoad::was_hit) {
            self.metrics.increment_uuid_cache_hits();
        } else {
            self.metrics.increment_uuid_cache_misses();
        }
        Ok(result?.into_value())
    }

    /// Reports whether the batch processor still accepts lookups.
    #[must_use]
    pub fn is_available(&self) -> bool {
        !self.sender.is_closed()
    }
}

pub struct BatchProcessor {
    lookup: BatchLookup,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl BatchProcessor {
    pub fn start(
        service: Arc<dyn MojangService>,
        cache: CacheManager,
        metrics: Arc<Metrics>,
        config: BatchConfig,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(config.queue_capacity.get());
        let lookup = BatchLookup {
            sender,
            cache: cache.clone(),
            metrics: Arc::clone(&metrics),
            client_lookups: Arc::default(),
        };
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(run_batch_processor(
            receiver,
            shutdown_receiver,
            service,
            cache,
            metrics,
            config,
        ));
        Self {
            lookup,
            shutdown: Some(shutdown),
            task,
        }
    }

    #[must_use]
    pub fn lookup(&self) -> BatchLookup {
        self.lookup.clone()
    }

    pub async fn shutdown(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "The batch processor may already have stopped and dropped its shutdown receiver."
            )]
            let _ = shutdown.send(());
        }
        match timeout(BATCH_SHUTDOWN_TIMEOUT, &mut self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => error!(%error, "batch processor stopped unexpectedly"),
            Err(_) => {
                warn!("batch processor did not drain before the shutdown deadline; aborting it");
                self.task.abort();
                // After aborting, wait for the task to stop and discard the join result.
                drop(self.task.await);
            }
        }
    }
}

async fn run_batch_processor(
    mut receiver: mpsc::Receiver<PendingLookup>,
    mut shutdown: oneshot::Receiver<()>,
    service: Arc<dyn MojangService>,
    cache: CacheManager,
    metrics: Arc<Metrics>,
    config: BatchConfig,
) {
    let mut pending = Vec::with_capacity(config.size.get());
    let mut tasks = JoinSet::new();
    let semaphore = Arc::new(Semaphore::new(config.max_in_flight_batches.get()));
    let mut ticker = interval(config.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await;
    let mut accepting = true;

    // After shutdown the receiver is closed, so `recv` returns the lookups that are still
    // queued and then `None`, which ends the loop.
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown, if accepting => {
                accepting = false;
                receiver.close();
            }
            lookup = receiver.recv() => {
                let Some(lookup) = lookup else {
                    break;
                };
                pending.push(lookup);
                if pending.len() >= config.size.get() {
                    spawn_batch(
                        &mut tasks,
                        std::mem::take(&mut pending),
                        Arc::clone(&service),
                        cache.clone(),
                        Arc::clone(&metrics),
                        Arc::clone(&semaphore),
                    )
                    .await;
                }
            }
            _ = ticker.tick() => {
                if !pending.is_empty() {
                    spawn_batch(
                        &mut tasks,
                        std::mem::take(&mut pending),
                        Arc::clone(&service),
                        cache.clone(),
                        Arc::clone(&metrics),
                        Arc::clone(&semaphore),
                    )
                    .await;
                }
            }
        }
    }

    if !pending.is_empty() {
        spawn_batch(&mut tasks, pending, service, cache, metrics, semaphore).await;
    }
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            error!(%error, "batch task stopped unexpectedly during shutdown");
        }
    }
}

async fn spawn_batch(
    tasks: &mut JoinSet<()>,
    batch: Vec<PendingLookup>,
    service: Arc<dyn MojangService>,
    cache: CacheManager,
    metrics: Arc<Metrics>,
    semaphore: Arc<Semaphore>,
) {
    #[expect(
        clippy::expect_used,
        reason = "The processor owns the semaphore and never closes it."
    )]
    let permit = semaphore
        .acquire_owned()
        .await
        .expect("the batch semaphore is never closed");
    // Reap finished batches here rather than in the main loop's `select!`, where the biased
    // receiver branch starves a `join_next` branch while the queue stays busy.
    while let Some(result) = tasks.try_join_next() {
        if let Err(error) = result {
            error!(%error, "batch task stopped unexpectedly");
        }
    }
    tasks.spawn(async move {
        let _permit = permit;
        process_batch(batch, service.as_ref(), &cache, &metrics).await;
    });
}

async fn process_batch(
    mut batch: Vec<PendingLookup>,
    service: &dyn MojangService,
    cache: &CacheManager,
    metrics: &Metrics,
) {
    batch.retain(|lookup| !lookup.response.is_closed());
    if batch.is_empty() {
        return;
    }

    let names: Vec<String> = batch.iter().map(|lookup| lookup.name.clone()).collect();
    metrics.record_batch(names.len());
    tracing::info!(usernames = names.len(), "processing username batch");

    let profiles = match service.lookup_names(&names).await {
        Ok(profiles) => profiles,
        Err(error) => {
            for lookup in batch {
                #[expect(
                    clippy::let_underscore_must_use,
                    reason = "The request may have been canceled before the error could be delivered."
                )]
                let _ = lookup.response.send(Err(error));
            }
            return;
        }
    };
    let profiles: HashMap<String, Uuid> = profiles
        .into_iter()
        .map(|(mut name, uuid)| {
            name.make_ascii_lowercase();
            (name, uuid)
        })
        .collect();

    for lookup in batch {
        let uuid = profiles.get(&lookup.name).copied();
        // Callers may have timed out or disconnected while Mojang answered, so the cache
        // stores the result for the next request instead of relying on the caller's load.
        cache.insert_uuid(lookup.name, uuid).await;
        #[expect(
            clippy::let_underscore_must_use,
            reason = "The request may have been canceled before the batch completed."
        )]
        let _ = lookup.response.send(Ok(uuid));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        net::{IpAddr, Ipv4Addr},
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use tokio::sync::{Semaphore, oneshot};
    use uuid::Uuid;

    use crate::{
        cache::CacheManager,
        metrics::Metrics,
        mojang::{MojangService, UpstreamError},
        rate_limit::ClientKey,
        types::SkinProperty,
    };

    use super::{
        BatchConfig, BatchConfigError, BatchLookup, BatchProcessor, ClientSlot, LOOKUP_TIMEOUT,
        MAX_LOOKUPS_PER_CLIENT, PendingLookup,
    };

    const CLIENT: ClientKey = ClientKey(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)));
    const OTHER_CLIENT: ClientKey = ClientKey(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2)));

    fn reserve(lookup: &BatchLookup) -> ClientSlot {
        lookup
            .client_lookups
            .reserve(CLIENT)
            .expect("the test client should have a free slot")
    }

    #[derive(Default)]
    struct FakeMojangService {
        profiles: HashMap<String, Uuid>,
        calls: Mutex<Vec<Vec<String>>>,
        error: Option<UpstreamError>,
    }

    struct BlockingMojangService {
        active: AtomicUsize,
        peak: AtomicUsize,
        release: Semaphore,
    }

    impl Default for BlockingMojangService {
        fn default() -> Self {
            Self {
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
                release: Semaphore::new(0),
            }
        }
    }

    #[async_trait]
    impl MojangService for FakeMojangService {
        async fn lookup_names(
            &self,
            names: &[String],
        ) -> Result<Vec<(String, Uuid)>, UpstreamError> {
            self.calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(names.to_vec());
            if let Some(error) = self.error {
                return Err(error);
            }
            Ok(names
                .iter()
                .filter_map(|name| {
                    self.profiles
                        .get(&name.to_ascii_lowercase())
                        .map(|uuid| (name.clone(), *uuid))
                })
                .collect())
        }

        #[expect(
            clippy::unreachable,
            reason = "A skin lookup is an unexpected call to the batch test double."
        )]
        async fn lookup_skin(&self, _uuid: Uuid) -> Result<Option<SkinProperty>, UpstreamError> {
            unreachable!("skin lookup is not used by batch tests")
        }
    }

    #[async_trait]
    impl MojangService for BlockingMojangService {
        async fn lookup_names(
            &self,
            _names: &[String],
        ) -> Result<Vec<(String, Uuid)>, UpstreamError> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            self.release
                .acquire()
                .await
                .expect("test semaphore should remain open")
                .forget();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        #[expect(
            clippy::unreachable,
            reason = "A skin lookup is an unexpected call to the batch test double."
        )]
        async fn lookup_skin(&self, _uuid: Uuid) -> Result<Option<SkinProperty>, UpstreamError> {
            unreachable!("skin lookup is not used by batch tests")
        }
    }

    fn test_config(size: usize) -> BatchConfig {
        BatchConfig::new(size, Duration::from_millis(10), 32, 2)
            .expect("test batch configuration should be valid")
    }

    async fn wait_until(condition: impl Fn() -> bool, message: &str) {
        tokio::time::timeout(Duration::from_secs(1), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect(message);
    }

    #[test]
    fn rejects_zero_batch_configuration_values() {
        assert!(matches!(
            BatchConfig::new(0, Duration::from_secs(1), 1, 1),
            Err(BatchConfigError::ZeroSize)
        ));
        assert!(matches!(
            BatchConfig::new(11, Duration::from_secs(1), 1, 1),
            Err(BatchConfigError::SizeTooLarge)
        ));
        assert!(matches!(
            BatchConfig::new(1, Duration::ZERO, 1, 1),
            Err(BatchConfigError::ZeroInterval)
        ));
        assert!(matches!(
            BatchConfig::new(1, Duration::from_secs(1), 0, 1),
            Err(BatchConfigError::ZeroQueueCapacity)
        ));
        assert!(matches!(
            BatchConfig::new(1, Duration::from_secs(1), 1, 0),
            Err(BatchConfigError::ZeroMaxInFlightBatches)
        ));
    }

    // The paused clock makes the wait measurable without taking real time.
    #[tokio::test(start_paused = true)]
    async fn default_config_sends_a_lone_lookup_within_a_second() {
        let service = Arc::new(FakeMojangService::default());
        let processor = BatchProcessor::start(
            Arc::<FakeMojangService>::clone(&service),
            CacheManager::default(),
            Arc::new(Metrics::default()),
            BatchConfig::default(),
        );
        let lookup = processor.lookup();

        let started = tokio::time::Instant::now();
        assert_eq!(
            lookup.lookup("Pistonmaster".to_owned(), CLIENT).await,
            Ok(None)
        );
        let elapsed = started.elapsed();
        assert!(elapsed <= Duration::from_secs(1), "waited {elapsed:?}");

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test]
    async fn batches_case_insensitive_results_and_negative_results() {
        let uuid = Uuid::parse_str("b1ae0778-4817-436c-96a3-a72c67cda060")
            .expect("test UUID should parse");
        let service = Arc::new(FakeMojangService {
            profiles: HashMap::from([("pistonmaster".to_owned(), uuid)]),
            ..FakeMojangService::default()
        });
        let cache = CacheManager::default();
        let processor = BatchProcessor::start(
            Arc::<FakeMojangService>::clone(&service),
            cache,
            Arc::new(Metrics::default()),
            test_config(10),
        );
        let lookup = processor.lookup();

        let (existing, missing) = tokio::join!(
            lookup.lookup("Pistonmaster".to_owned(), CLIENT),
            lookup.lookup("MissingPlayer".to_owned(), CLIENT)
        );
        assert_eq!(existing.expect("lookup should succeed"), Some(uuid));
        assert_eq!(missing.expect("lookup should succeed"), None);

        assert_eq!(
            lookup
                .lookup("PISTONMASTER".to_owned(), CLIENT)
                .await
                .expect("cached lookup should succeed"),
            Some(uuid)
        );
        assert_eq!(
            service
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test]
    async fn coalesces_duplicate_names_before_batching() {
        let service = Arc::new(FakeMojangService::default());
        let processor = BatchProcessor::start(
            Arc::<FakeMojangService>::clone(&service),
            CacheManager::default(),
            Arc::new(Metrics::default()),
            test_config(10),
        );
        let lookup = processor.lookup();

        let (first, second, third) = tokio::join!(
            lookup.lookup("Pistonmaster".to_owned(), CLIENT),
            lookup.lookup("PISTONMASTER".to_owned(), CLIENT),
            lookup.lookup("pistonmaster".to_owned(), CLIENT),
        );
        assert_eq!(first, Ok(None));
        assert_eq!(second, Ok(None));
        assert_eq!(third, Ok(None));

        {
            let calls = service
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(calls.as_slice(), &[vec!["pistonmaster".to_owned()]]);
            drop(calls);
        }

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn flushes_immediately_when_batch_is_full() {
        let service = Arc::new(FakeMojangService::default());
        let processor = BatchProcessor::start(
            Arc::<FakeMojangService>::clone(&service),
            CacheManager::default(),
            Arc::new(Metrics::default()),
            BatchConfig::new(2, Duration::from_secs(30), 32, 2)
                .expect("test batch configuration should be valid"),
        );
        let lookup = processor.lookup();

        let (_first, _second) = tokio::time::timeout(Duration::from_millis(250), async {
            tokio::join!(
                lookup.lookup("First".to_owned(), CLIENT),
                lookup.lookup("Second".to_owned(), CLIENT)
            )
        })
        .await
        .expect("the full batch should complete before the timeout");
        assert_eq!(
            service
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test]
    async fn propagates_typed_upstream_errors() {
        let service = Arc::new(FakeMojangService {
            error: Some(UpstreamError::Timeout),
            ..FakeMojangService::default()
        });
        let metrics = Arc::new(Metrics::default());
        let processor = BatchProcessor::start(
            service,
            CacheManager::default(),
            Arc::clone(&metrics),
            test_config(10),
        );
        let lookup = processor.lookup();

        assert_eq!(
            lookup.lookup("Pistonmaster".to_owned(), CLIENT).await,
            Err(UpstreamError::Timeout)
        );
        let snapshot = metrics.snapshot();
        assert_eq!(
            (snapshot.uuid_cache_hits, snapshot.uuid_cache_misses),
            (0, 1),
            "a failed lookup still counts as a cache miss"
        );

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_lookups_that_mojang_does_not_answer() {
        let service = Arc::new(BlockingMojangService::default());
        let processor = BatchProcessor::start(
            Arc::<BlockingMojangService>::clone(&service),
            CacheManager::default(),
            Arc::new(Metrics::default()),
            test_config(1),
        );
        let lookup = processor.lookup();

        let started = tokio::time::Instant::now();
        assert_eq!(
            lookup.lookup("Pistonmaster".to_owned(), CLIENT).await,
            Err(UpstreamError::Timeout)
        );
        assert!(started.elapsed() >= LOOKUP_TIMEOUT);

        service.release.add_permits(1);
        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test]
    async fn skips_lookups_that_were_canceled_while_queued() {
        let service = Arc::new(FakeMojangService::default());
        let processor = BatchProcessor::start(
            Arc::<FakeMojangService>::clone(&service),
            CacheManager::default(),
            Arc::new(Metrics::default()),
            BatchConfig::new(2, Duration::from_secs(30), 32, 2)
                .expect("test batch configuration should be valid"),
        );
        let lookup = processor.lookup();
        let (response, receiver) = oneshot::channel();
        drop(receiver);
        assert!(
            lookup
                .sender
                .try_send(PendingLookup {
                    name: "canceled".to_owned(),
                    response,
                    _slot: reserve(&lookup),
                })
                .is_ok(),
            "the queue should accept the canceled lookup"
        );

        // The second lookup fills the batch.
        assert_eq!(
            lookup.lookup("Pistonmaster".to_owned(), CLIENT).await,
            Ok(None)
        );
        assert_eq!(
            service
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[vec!["pistonmaster".to_owned()]],
            "Mojang should not be asked for names that nobody waits for"
        );

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test]
    async fn applies_backpressure_at_the_in_flight_batch_limit() {
        let service = Arc::new(BlockingMojangService::default());
        let processor = BatchProcessor::start(
            Arc::<BlockingMojangService>::clone(&service),
            CacheManager::default(),
            Arc::new(Metrics::default()),
            BatchConfig::new(1, Duration::from_secs(30), 3, 1)
                .expect("test batch configuration should be valid"),
        );
        let lookup = processor.lookup();
        let requests = ["First", "Second", "Third"].map(|name| {
            let lookup = lookup.clone();
            tokio::spawn(async move { lookup.lookup(name.to_owned(), CLIENT).await })
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            while service.active.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the first batch should start");
        tokio::task::yield_now().await;
        assert_eq!(service.peak.load(Ordering::SeqCst), 1);

        service.release.add_permits(requests.len());
        for request in requests {
            request
                .await
                .expect("lookup task should finish")
                .expect("lookup should succeed");
        }
        assert_eq!(service.peak.load(Ordering::SeqCst), 1);

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test]
    async fn rejects_lookups_when_the_queue_is_full() {
        let service = Arc::new(BlockingMojangService::default());
        let metrics = Arc::new(Metrics::default());
        let processor = BatchProcessor::start(
            Arc::<BlockingMojangService>::clone(&service),
            CacheManager::default(),
            Arc::clone(&metrics),
            BatchConfig::new(1, Duration::from_secs(30), 1, 1)
                .expect("test batch configuration should be valid"),
        );
        let lookup = processor.lookup();
        let queue = |name: &str| {
            let (response, receiver) = oneshot::channel();
            let sent = lookup.sender.try_send(PendingLookup {
                name: name.to_owned(),
                response,
                _slot: reserve(&lookup),
            });
            assert!(sent.is_ok(), "the queue should accept {name}");
            receiver
        };
        // The first batch holds the only in-flight permit.
        let running = queue("first");
        wait_until(
            || service.active.load(Ordering::SeqCst) == 1,
            "the first batch should start",
        )
        .await;
        // The processor takes the second lookup and blocks while it waits for a permit.
        let waiting = queue("second");
        wait_until(
            || lookup.sender.capacity() == 1,
            "the processor should take the second lookup",
        )
        .await;
        // The third lookup fills the queue, so the next one is rejected.
        let queued = queue("third");
        assert_eq!(
            lookup.lookup("Fourth".to_owned(), CLIENT).await,
            Err(UpstreamError::Overloaded)
        );
        assert_eq!(metrics.snapshot().batch_queue_rejections, 1);

        service.release.add_permits(3);
        for receiver in [running, waiting, queued] {
            assert_eq!(
                receiver.await.expect("the batch should answer the lookup"),
                Ok(None)
            );
        }
        assert_eq!(service.peak.load(Ordering::SeqCst), 1);

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test]
    async fn limits_how_many_lookups_one_client_holds() {
        let service = Arc::new(BlockingMojangService::default());
        let processor = BatchProcessor::start(
            Arc::<BlockingMojangService>::clone(&service),
            CacheManager::default(),
            Arc::new(Metrics::default()),
            BatchConfig::new(1, Duration::from_secs(30), 1_024, 1)
                .expect("test batch configuration should be valid"),
        );
        let lookup = processor.lookup();
        let spawn_lookup = |name: String, client| {
            let lookup = lookup.clone();
            tokio::spawn(async move { lookup.lookup(name, client).await })
        };
        let mut requests: Vec<_> = (0..MAX_LOOKUPS_PER_CLIENT)
            .map(|index| spawn_lookup(format!("player{index}"), CLIENT))
            .collect();
        wait_until(
            || lookup.client_lookups.held_by(CLIENT) == MAX_LOOKUPS_PER_CLIENT,
            "the client should fill its share of the queue",
        )
        .await;

        assert_eq!(
            lookup.lookup("OneTooMany".to_owned(), CLIENT).await,
            Err(UpstreamError::Overloaded)
        );
        requests.push(spawn_lookup("Other".to_owned(), OTHER_CLIENT));
        wait_until(
            || lookup.client_lookups.held_by(OTHER_CLIENT) == 1,
            "another client should still queue a lookup",
        )
        .await;

        service.release.add_permits(requests.len());
        for request in requests {
            assert_eq!(request.await.expect("lookup task should finish"), Ok(None));
        }
        assert_eq!(lookup.client_lookups.held_by(CLIENT), 0);
        assert!(
            lookup
                .client_lookups
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "answered lookups release every slot"
        );

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn caches_results_that_arrive_after_the_caller_timed_out() {
        let service = Arc::new(BlockingMojangService::default());
        let cache = CacheManager::default();
        let processor = BatchProcessor::start(
            Arc::<BlockingMojangService>::clone(&service),
            cache.clone(),
            Arc::new(Metrics::default()),
            test_config(1),
        );
        let lookup = processor.lookup();

        assert_eq!(
            lookup.lookup("Pistonmaster".to_owned(), CLIENT).await,
            Err(UpstreamError::Timeout)
        );
        service.release.add_permits(1);
        drop(lookup);
        // Shutdown waits for the running batch to finish.
        processor.shutdown().await;

        let cached = cache
            .get_or_try_insert_uuid("pistonmaster".to_owned(), async { Err::<_, ()>(()) })
            .await
            .expect("the late result should be cached");
        assert!(cached.was_hit());
        assert_eq!(cached.into_value(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_answers_lookups_that_are_still_queued() {
        let service = Arc::new(FakeMojangService::default());
        let processor = BatchProcessor::start(
            Arc::<FakeMojangService>::clone(&service),
            CacheManager::default(),
            Arc::new(Metrics::default()),
            BatchConfig::new(10, Duration::from_secs(30), 32, 2)
                .expect("test batch configuration should be valid"),
        );
        let lookup = processor.lookup();
        let requests = ["First", "Second"].map(|name| {
            let lookup = lookup.clone();
            tokio::spawn(async move { lookup.lookup(name.to_owned(), CLIENT).await })
        });
        // With paused time, the clock only advances once every task waits, so both lookups
        // reach the processor long before the batch interval elapses.
        tokio::time::sleep(Duration::from_millis(1)).await;

        tokio::time::timeout(Duration::from_secs(1), processor.shutdown())
            .await
            .expect("shutdown should flush the pending batch without waiting for the interval");
        for request in requests {
            assert_eq!(request.await.expect("lookup task should finish"), Ok(None));
        }
        assert_eq!(
            service
                .calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_closes_external_lookup_handles() {
        let processor = BatchProcessor::start(
            Arc::new(FakeMojangService::default()),
            CacheManager::default(),
            Arc::new(Metrics::default()),
            test_config(10),
        );
        let lookup = processor.lookup();

        tokio::time::timeout(Duration::from_millis(250), processor.shutdown())
            .await
            .expect("shutdown should not wait for external lookup handles");
        assert!(
            !lookup.is_available(),
            "lookups are unavailable after shutdown"
        );
        assert_eq!(
            lookup.lookup("after-shutdown".to_owned(), CLIENT).await,
            Err(UpstreamError::BatchStopped)
        );
    }
}
