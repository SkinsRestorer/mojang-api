use std::{future::Future, sync::Arc, time::Duration};

use moka::{Expiry, future::Cache};
use thiserror::Error;
use uuid::Uuid;

use crate::types::SkinProperty;

const NAME_CACHE_CAPACITY: u64 = 10_000;
const SKIN_CACHE_CAPACITY_BYTES: u64 = 32 * 1024 * 1024;
const POSITIVE_CACHE_TTL: Duration = Duration::from_hours(6);
const NEGATIVE_CACHE_TTL: Duration = Duration::from_mins(15);
const ESTIMATED_SKIN_ENTRY_BYTES: u64 = 2 * 1024;
const SKIN_ENTRY_OVERHEAD_BYTES: usize = 128;
const MAX_CACHE_TTL: Duration = Duration::from_hours(8_760_000);

#[derive(Debug, Clone)]
pub struct CacheManager {
    names: Cache<String, Option<Uuid>>,
    skins: Cache<Uuid, Option<Arc<SkinProperty>>>,
}

impl Default for CacheManager {
    fn default() -> Self {
        Self::build(
            NAME_CACHE_CAPACITY,
            SKIN_CACHE_CAPACITY_BYTES,
            POSITIVE_CACHE_TTL,
            NEGATIVE_CACHE_TTL,
        )
    }
}

impl CacheManager {
    /// Creates caches sized for the requested number of typical entries.
    ///
    /// # Errors
    ///
    /// Returns an error when the time to live is zero or exceeds Moka's supported maximum.
    pub fn new(capacity: u64, ttl: Duration) -> Result<Self, CacheConfigError> {
        let skin_capacity_bytes = capacity.saturating_mul(ESTIMATED_SKIN_ENTRY_BYTES);
        Self::with_limits(
            capacity,
            skin_capacity_bytes,
            ttl,
            ttl.min(NEGATIVE_CACHE_TTL),
        )
    }

    /// Creates caches with independent name-entry and skin-byte limits.
    ///
    /// # Errors
    ///
    /// Returns an error when either time to live is zero or exceeds Moka's supported maximum.
    pub fn with_limits(
        name_capacity: u64,
        skin_capacity_bytes: u64,
        positive_ttl: Duration,
        negative_ttl: Duration,
    ) -> Result<Self, CacheConfigError> {
        validate_ttl(positive_ttl)?;
        validate_ttl(negative_ttl)?;
        Ok(Self::build(
            name_capacity,
            skin_capacity_bytes,
            positive_ttl,
            negative_ttl,
        ))
    }

    fn build(
        name_capacity: u64,
        skin_capacity_bytes: u64,
        positive_ttl: Duration,
        negative_ttl: Duration,
    ) -> Self {
        let expiry = OptionalValueExpiry {
            positive_ttl,
            negative_ttl,
        };
        Self {
            names: Cache::builder()
                .max_capacity(name_capacity)
                .expire_after(expiry)
                .build(),
            skins: Cache::builder()
                .weigher(|uuid: &Uuid, property: &Option<Arc<SkinProperty>>| {
                    skin_entry_weight(uuid, property.as_ref())
                })
                .max_capacity(skin_capacity_bytes)
                .expire_after(expiry)
                .build(),
        }
    }

    /// Returns the cached UUID or coalesces concurrent initialization for the same name.
    ///
    /// # Errors
    ///
    /// Returns the shared initialization error when the loader fails.
    pub async fn get_or_try_insert_uuid<E>(
        &self,
        normalized_name: String,
        init: impl Future<Output = Result<Option<Uuid>, E>>,
    ) -> Result<CacheLoad<Option<Uuid>>, Arc<E>>
    where
        E: Send + Sync + 'static,
    {
        let entry = self
            .names
            .entry(normalized_name)
            .or_try_insert_with(init)
            .await?;
        Ok(CacheLoad {
            loaded: entry.is_fresh(),
            value: entry.into_value(),
        })
    }

    /// Returns the cached skin or coalesces concurrent initialization for the same UUID.
    ///
    /// # Errors
    ///
    /// Returns the shared initialization error when the loader fails.
    pub async fn get_or_try_insert_skin<E>(
        &self,
        uuid: Uuid,
        init: impl Future<Output = Result<Option<Arc<SkinProperty>>, E>>,
    ) -> Result<CacheLoad<Option<Arc<SkinProperty>>>, Arc<E>>
    where
        E: Send + Sync + 'static,
    {
        let entry = self.skins.entry(uuid).or_try_insert_with(init).await?;
        Ok(CacheLoad {
            loaded: entry.is_fresh(),
            value: entry.into_value(),
        })
    }

    pub async fn clear(&self) {
        self.names.invalidate_all();
        self.skins.invalidate_all();
        self.names.run_pending_tasks().await;
        self.skins.run_pending_tasks().await;
    }
}

#[derive(Debug)]
pub struct CacheLoad<T> {
    value: T,
    loaded: bool,
}

impl<T> CacheLoad<T> {
    #[must_use]
    pub fn was_loaded(&self) -> bool {
        self.loaded
    }

    #[must_use]
    pub fn into_value(self) -> T {
        self.value
    }
}

#[derive(Debug, Clone, Copy)]
struct OptionalValueExpiry {
    positive_ttl: Duration,
    negative_ttl: Duration,
}

impl<K, V> Expiry<K, Option<V>> for OptionalValueExpiry {
    fn expire_after_create(
        &self,
        _key: &K,
        value: &Option<V>,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        Some(self.ttl_for(value.as_ref()))
    }

    fn expire_after_update(
        &self,
        _key: &K,
        value: &Option<V>,
        _updated_at: std::time::Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(self.ttl_for(value.as_ref()))
    }
}

impl OptionalValueExpiry {
    fn ttl_for<T>(&self, value: Option<&T>) -> Duration {
        if value.is_some() {
            self.positive_ttl
        } else {
            self.negative_ttl
        }
    }
}

fn skin_entry_weight(_uuid: &Uuid, property: Option<&Arc<SkinProperty>>) -> u32 {
    let payload_bytes = property.map_or(0, |property| {
        property
            .value
            .capacity()
            .saturating_add(property.signature.capacity())
    });
    u32::try_from(SKIN_ENTRY_OVERHEAD_BYTES.saturating_add(payload_bytes)).unwrap_or(u32::MAX)
}

fn validate_ttl(ttl: Duration) -> Result<(), CacheConfigError> {
    if ttl.is_zero() {
        return Err(CacheConfigError::ZeroTimeToLive);
    }
    if ttl > MAX_CACHE_TTL {
        return Err(CacheConfigError::TimeToLiveTooLong);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum CacheConfigError {
    #[error("cache time to live must be greater than zero")]
    ZeroTimeToLive,
    #[error("cache time to live must not exceed 1000 years")]
    TimeToLiveTooLong,
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use uuid::Uuid;

    use crate::types::SkinProperty;

    use super::{CacheConfigError, CacheManager, MAX_CACHE_TTL, OptionalValueExpiry};

    #[test]
    fn rejects_unsupported_cache_time_to_live() {
        assert!(matches!(
            CacheManager::new(1, Duration::ZERO),
            Err(CacheConfigError::ZeroTimeToLive)
        ));
        assert!(matches!(
            CacheManager::new(1, MAX_CACHE_TTL.saturating_add(Duration::from_secs(1))),
            Err(CacheConfigError::TimeToLiveTooLong)
        ));
    }

    #[test]
    fn expires_negative_results_sooner() {
        let expiry = OptionalValueExpiry {
            positive_ttl: Duration::from_hours(6),
            negative_ttl: Duration::from_mins(15),
        };

        assert_eq!(expiry.ttl_for(Some(&())), Duration::from_hours(6));
        assert_eq!(expiry.ttl_for::<()>(None), Duration::from_mins(15));
    }

    #[tokio::test]
    async fn coalesces_concurrent_loads_for_the_same_key() {
        let cache = CacheManager::new(32, Duration::from_mins(1))
            .expect("test cache configuration should be valid");
        let loads = Arc::new(AtomicUsize::new(0));
        let load = || {
            let loads = Arc::clone(&loads);
            async move {
                loads.fetch_add(1, Ordering::Relaxed);
                tokio::task::yield_now().await;
                Ok::<_, ()>(Some(Uuid::nil()))
            }
        };

        let (first, second, third) = tokio::join!(
            cache.get_or_try_insert_uuid("player".to_owned(), load()),
            cache.get_or_try_insert_uuid("player".to_owned(), load()),
            cache.get_or_try_insert_uuid("player".to_owned(), load()),
        );
        let results = [
            first.expect("cache load should succeed"),
            second.expect("cache load should succeed"),
            third.expect("cache load should succeed"),
        ];

        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(
            results.iter().filter(|result| result.was_loaded()).count(),
            1
        );
        assert!(
            results
                .into_iter()
                .all(|result| result.into_value() == Some(Uuid::nil()))
        );
    }

    #[tokio::test]
    async fn shares_cached_skin_strings() {
        let cache = CacheManager::new(32, Duration::from_mins(1))
            .expect("test cache configuration should be valid");
        let uuid = Uuid::nil();
        let property = Arc::new(SkinProperty {
            value: "texture".to_owned(),
            signature: "signature".to_owned(),
        });

        let first = cache
            .get_or_try_insert_skin(uuid, async { Ok::<_, ()>(Some(Arc::clone(&property))) })
            .await
            .expect("cache load should succeed")
            .into_value()
            .expect("skin should exist");
        let second = cache
            .get_or_try_insert_skin(uuid, async { Ok::<_, ()>(None) })
            .await
            .expect("cache lookup should succeed")
            .into_value()
            .expect("skin should exist");

        assert!(Arc::ptr_eq(&first, &second));
    }
}
