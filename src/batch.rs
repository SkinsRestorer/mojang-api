use std::{collections::HashMap, num::NonZeroUsize, sync::Arc, time::Duration};

use thiserror::Error;
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::{JoinHandle, JoinSet},
    time::{MissedTickBehavior, interval, timeout},
};
use tracing::{error, warn};
use uuid::Uuid;

use crate::{cache::CacheManager, metrics::Metrics, mojang::MojangService, mojang::UpstreamError};

const BATCH_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy)]
pub struct BatchConfig {
    size: NonZeroUsize,
    interval: Duration,
    queue_capacity: NonZeroUsize,
    max_in_flight_batches: NonZeroUsize,
}

impl BatchConfig {
    /// Creates batching limits.
    ///
    /// # Errors
    ///
    /// Returns an error when any limit or the batching interval is zero.
    pub fn new(
        size: usize,
        interval: Duration,
        queue_capacity: usize,
        max_in_flight_batches: usize,
    ) -> Result<Self, BatchConfigError> {
        let size = NonZeroUsize::new(size).ok_or(BatchConfigError::ZeroSize)?;
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
}

#[derive(Debug, Clone)]
pub struct BatchLookup {
    sender: mpsc::Sender<PendingLookup>,
    cache: CacheManager,
    metrics: Arc<Metrics>,
}

impl BatchLookup {
    /// Resolves a username from the cache or the next Mojang batch.
    ///
    /// # Errors
    ///
    /// Returns an upstream error when the queue is unavailable or Mojang cannot resolve the batch.
    pub async fn lookup(&self, mut name: String) -> Result<Option<Uuid>, UpstreamError> {
        name.make_ascii_lowercase();
        let request_name = name.clone();
        let sender = self.sender.clone();
        let metrics = Arc::clone(&self.metrics);
        let result = self
            .cache
            .get_or_try_insert_uuid(name, async move {
                metrics.increment_uuid_cache_misses();
                let (response, receiver) = oneshot::channel();
                sender
                    .send(PendingLookup {
                        name: request_name,
                        response,
                    })
                    .await
                    .map_err(|_| UpstreamError::Transport)?;
                receiver.await.map_err(|_| UpstreamError::Transport)?
            })
            .await
            .map_err(|error| *error)?;

        if !result.was_loaded() {
            self.metrics.increment_uuid_cache_hits();
        }
        Ok(result.into_value())
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
            cache,
            metrics: Arc::clone(&metrics),
        };
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let task = tokio::spawn(run_batch_processor(
            receiver,
            shutdown_receiver,
            service,
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
            let _ = shutdown.send(());
        }
        drop(self.lookup);
        match timeout(BATCH_SHUTDOWN_TIMEOUT, &mut self.task).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => error!(%error, "batch processor stopped unexpectedly"),
            Err(_) => {
                warn!("batch processor did not drain before the shutdown deadline; aborting it");
                self.task.abort();
                let _ = self.task.await;
            }
        }
    }
}

async fn run_batch_processor(
    mut receiver: mpsc::Receiver<PendingLookup>,
    mut shutdown: oneshot::Receiver<()>,
    service: Arc<dyn MojangService>,
    metrics: Arc<Metrics>,
    config: BatchConfig,
) {
    let mut pending = Vec::with_capacity(config.size.get());
    let mut tasks = JoinSet::new();
    let semaphore = Arc::new(Semaphore::new(config.max_in_flight_batches.get()));
    let mut ticker = interval(config.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => {
                receiver.close();
                break;
            }
            lookup = receiver.recv() => {
                let Some(lookup) = lookup else {
                    break;
                };
                pending.push(lookup);
                if pending.len() >= config.size.get() {
                    spawn_batch(
                        &mut tasks,
                        std::mem::replace(
                            &mut pending,
                            Vec::with_capacity(config.size.get()),
                        ),
                        Arc::clone(&service),
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
                        std::mem::replace(
                            &mut pending,
                            Vec::with_capacity(config.size.get()),
                        ),
                        Arc::clone(&service),
                        Arc::clone(&metrics),
                        Arc::clone(&semaphore),
                    )
                    .await;
                }
            }
            completed = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = completed {
                    error!(%error, "batch task stopped unexpectedly");
                }
            }
        }
    }

    while let Some(lookup) = receiver.recv().await {
        pending.push(lookup);
        if pending.len() >= config.size.get() {
            spawn_batch(
                &mut tasks,
                std::mem::replace(&mut pending, Vec::with_capacity(config.size.get())),
                Arc::clone(&service),
                Arc::clone(&metrics),
                Arc::clone(&semaphore),
            )
            .await;
        }
    }

    if !pending.is_empty() {
        spawn_batch(&mut tasks, pending, service, metrics, semaphore).await;
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
    metrics: Arc<Metrics>,
    semaphore: Arc<Semaphore>,
) {
    let Ok(permit) = semaphore.acquire_owned().await else {
        reject_batch(batch, UpstreamError::Transport);
        return;
    };
    tasks.spawn(async move {
        let _permit = permit;
        process_batch(batch, service.as_ref(), &metrics).await;
    });
}

async fn process_batch(
    mut batch: Vec<PendingLookup>,
    service: &dyn MojangService,
    metrics: &Metrics,
) {
    batch.retain(|lookup| !lookup.response.is_closed());
    if batch.is_empty() {
        return;
    }

    let (names, responses): (Vec<_>, Vec<_>) = batch
        .into_iter()
        .map(|lookup| (lookup.name, lookup.response))
        .unzip();
    metrics.record_batch(names.len());
    tracing::info!(usernames = names.len(), "processing username batch");

    let profiles = match service.lookup_names(&names).await {
        Ok(profiles) => profiles,
        Err(error) => {
            reject_responses(responses, error);
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

    for (name, response) in names.into_iter().zip(responses) {
        let uuid = profiles.get(&name).copied();
        let _ = response.send(Ok(uuid));
    }
}

fn reject_batch(batch: Vec<PendingLookup>, error: UpstreamError) {
    reject_responses(batch.into_iter().map(|lookup| lookup.response), error);
}

fn reject_responses(
    responses: impl IntoIterator<Item = oneshot::Sender<Result<Option<Uuid>, UpstreamError>>>,
    error: UpstreamError,
) {
    for response in responses {
        let _ = response.send(Err(error));
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use tokio::sync::Semaphore;
    use uuid::Uuid;

    use crate::{
        cache::CacheManager,
        metrics::Metrics,
        mojang::{MojangService, UpstreamError},
        types::SkinProperty,
    };

    use super::{BatchConfig, BatchConfigError, BatchProcessor};

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
            let permit = self
                .release
                .acquire()
                .await
                .expect("test semaphore should remain open");
            permit.forget();
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(Vec::new())
        }

        async fn lookup_skin(&self, _uuid: Uuid) -> Result<Option<SkinProperty>, UpstreamError> {
            unreachable!("skin lookup is not used by batch tests")
        }
    }

    fn test_config(size: usize) -> BatchConfig {
        BatchConfig::new(size, Duration::from_millis(10), 32, 2)
            .expect("test batch configuration should be valid")
    }

    #[test]
    fn rejects_zero_batch_configuration_values() {
        assert!(matches!(
            BatchConfig::new(0, Duration::from_secs(1), 1, 1),
            Err(BatchConfigError::ZeroSize)
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

    #[tokio::test]
    async fn batches_case_insensitive_results_and_negative_results() {
        let uuid = Uuid::parse_str("b1ae0778-4817-436c-96a3-a72c67cda060")
            .expect("test UUID should parse");
        let service = Arc::new(FakeMojangService {
            profiles: HashMap::from([("pistonmaster".to_owned(), uuid)]),
            ..FakeMojangService::default()
        });
        let cache = CacheManager::new(32, Duration::from_mins(1))
            .expect("test cache configuration should be valid");
        let processor = BatchProcessor::start(
            service.clone(),
            cache,
            Arc::new(Metrics::default()),
            test_config(10),
        );
        let lookup = processor.lookup();

        let (existing, missing) = tokio::join!(
            lookup.lookup("Pistonmaster".to_owned()),
            lookup.lookup("MissingPlayer".to_owned())
        );
        assert_eq!(existing.expect("lookup should succeed"), Some(uuid));
        assert_eq!(missing.expect("lookup should succeed"), None);

        assert_eq!(
            lookup
                .lookup("PISTONMASTER".to_owned())
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
            service.clone(),
            CacheManager::new(32, Duration::from_mins(1))
                .expect("test cache configuration should be valid"),
            Arc::new(Metrics::default()),
            test_config(10),
        );
        let lookup = processor.lookup();

        let (first, second, third) = tokio::join!(
            lookup.lookup("Pistonmaster".to_owned()),
            lookup.lookup("PISTONMASTER".to_owned()),
            lookup.lookup("pistonmaster".to_owned()),
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
        }

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test]
    async fn flushes_immediately_when_batch_is_full() {
        let service = Arc::new(FakeMojangService::default());
        let processor = BatchProcessor::start(
            service.clone(),
            CacheManager::new(32, Duration::from_mins(1))
                .expect("test cache configuration should be valid"),
            Arc::new(Metrics::default()),
            BatchConfig::new(2, Duration::from_secs(30), 32, 2)
                .expect("test batch configuration should be valid"),
        );
        let lookup = processor.lookup();

        let completed = tokio::time::timeout(Duration::from_millis(250), async {
            tokio::join!(
                lookup.lookup("First".to_owned()),
                lookup.lookup("Second".to_owned())
            )
        })
        .await;
        assert!(completed.is_ok());
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
        let processor = BatchProcessor::start(
            service,
            CacheManager::new(32, Duration::from_mins(1))
                .expect("test cache configuration should be valid"),
            Arc::new(Metrics::default()),
            test_config(10),
        );
        let lookup = processor.lookup();

        assert_eq!(
            lookup.lookup("Pistonmaster".to_owned()).await,
            Err(UpstreamError::Timeout)
        );

        drop(lookup);
        processor.shutdown().await;
    }

    #[tokio::test]
    async fn applies_backpressure_at_the_in_flight_batch_limit() {
        let service = Arc::new(BlockingMojangService::default());
        let processor = BatchProcessor::start(
            service.clone(),
            CacheManager::new(32, Duration::from_mins(1))
                .expect("test cache configuration should be valid"),
            Arc::new(Metrics::default()),
            BatchConfig::new(1, Duration::from_secs(30), 2, 1)
                .expect("test batch configuration should be valid"),
        );
        let lookup = processor.lookup();
        let requests = ["First", "Second", "Third"].map(|name| {
            let lookup = lookup.clone();
            tokio::spawn(async move { lookup.lookup(name.to_owned()).await })
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
    async fn shutdown_closes_external_lookup_handles() {
        let processor = BatchProcessor::start(
            Arc::new(FakeMojangService::default()),
            CacheManager::new(32, Duration::from_mins(1))
                .expect("test cache configuration should be valid"),
            Arc::new(Metrics::default()),
            test_config(10),
        );
        let lookup = processor.lookup();

        tokio::time::timeout(Duration::from_millis(250), processor.shutdown())
            .await
            .expect("shutdown should not wait for external lookup handles");
        assert_eq!(
            lookup.lookup("after-shutdown".to_owned()).await,
            Err(UpstreamError::Transport)
        );
    }
}
