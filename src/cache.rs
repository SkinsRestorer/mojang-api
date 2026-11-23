use std::{future::Future, hash::Hash, sync::Arc, time::Duration};

use moka::{Expiry, future::Cache};
use uuid::Uuid;

use crate::types::SkinProperty;

const NAME_CACHE_CAPACITY: u64 = 100_000;
const SKIN_CACHE_CAPACITY_BYTES: u64 = 32 * 1024 * 1024;
/// A name keeps its UUID until the player renames, which Mojang limits to every 30 days.
const POSITIVE_NAME_TTL: Duration = Duration::from_hours(6);
/// Players can change skins at any time, so skins expire sooner than names.
const POSITIVE_SKIN_TTL: Duration = Duration::from_hours(1);
const NEGATIVE_CACHE_TTL: Duration = Duration::from_mins(15);
const SKIN_ENTRY_OVERHEAD_BYTES: usize = 128;

/// Caches name-to-UUID and UUID-to-skin lookups.
///
/// The two caches expire independently, so a cached name and the cached skin for its UUID
/// may come from different points in time.
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
            OptionalValueExpiry {
                positive_ttl: POSITIVE_NAME_TTL,
                negative_ttl: NEGATIVE_CACHE_TTL,
            },
            OptionalValueExpiry {
                positive_ttl: POSITIVE_SKIN_TTL,
                negative_ttl: NEGATIVE_CACHE_TTL,
            },
        )
    }
}

impl CacheManager {
    fn build(
        name_capacity: u64,
        skin_capacity_bytes: u64,
        name_expiry: OptionalValueExpiry,
        skin_expiry: OptionalValueExpiry,
    ) -> Self {
        Self {
            names: Cache::builder()
                .max_capacity(name_capacity)
                .expire_after(name_expiry)
                .build(),
            skins: Cache::builder()
                .weigher(|_uuid: &Uuid, property: &Option<Arc<SkinProperty>>| {
                    skin_entry_weight(property.as_deref())
                })
                .max_capacity(skin_capacity_bytes)
                .expire_after(skin_expiry)
                .build(),
        }
    }

    /// Returns the cached UUID or coalesces concurrent initialization for the same name.
    ///
    /// The result is a hit only when the value was already cached. Requests that wait for
    /// another request's in-flight load count as misses.
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
        get_or_try_insert(&self.names, normalized_name, init).await
    }

    /// Stores a username lookup result that no request may be waiting for.
    pub async fn insert_uuid(&self, normalized_name: String, uuid: Option<Uuid>) {
        self.names.insert(normalized_name, uuid).await;
    }

    /// Returns the cached skin or coalesces concurrent initialization for the same UUID.
    ///
    /// Hits follow the same rules as [`Self::get_or_try_insert_uuid`].
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
        get_or_try_insert(&self.skins, uuid, init).await
    }
}

async fn get_or_try_insert<K, V, E>(
    cache: &Cache<K, V>,
    key: K,
    init: impl Future<Output = Result<V, E>>,
) -> Result<CacheLoad<V>, Arc<E>>
where
    K: Hash + Eq + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    E: Send + Sync + 'static,
{
    if let Some(value) = cache.get(&key).await {
        return Ok(CacheLoad { value, hit: true });
    }
    let entry = cache.entry(key).or_try_insert_with(init).await?;
    Ok(CacheLoad {
        value: entry.into_value(),
        hit: false,
    })
}

#[derive(Debug)]
pub struct CacheLoad<T> {
    value: T,
    hit: bool,
}

impl<T> CacheLoad<T> {
    /// Reports whether the value was already cached.
    #[must_use]
    pub fn was_hit(&self) -> bool {
        self.hit
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
        Some(if value.is_some() {
            self.positive_ttl
        } else {
            self.negative_ttl
        })
    }
}

fn skin_entry_weight(property: Option<&SkinProperty>) -> u32 {
    let payload_bytes = property.map_or(0, |property| {
        property
            .value
            .capacity()
            .saturating_add(property.signature.capacity())
    });
    u32::try_from(SKIN_ENTRY_OVERHEAD_BYTES.saturating_add(payload_bytes)).unwrap_or(u32::MAX)
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

    use super::{CacheManager, OptionalValueExpiry};

    #[tokio::test]
    async fn expires_negative_results_before_positive_results() {
        let expiry = OptionalValueExpiry {
            positive_ttl: Duration::from_secs(60),
            negative_ttl: Duration::from_millis(50),
        };
        let cache = CacheManager::build(100, 1024 * 1024, expiry, expiry);
        let found = Uuid::from_u128(1);
        let load = |value| async move { Ok::<_, ()>(value) };

        for (name, value) in [("found", Some(found)), ("missing", None)] {
            let result = cache
                .get_or_try_insert_uuid(name.to_owned(), load(value))
                .await
                .expect("cache load should succeed");
            assert!(!result.was_hit());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;

        let found_again = cache
            .get_or_try_insert_uuid("found".to_owned(), load(None))
            .await
            .expect("cache lookup should succeed");
        assert!(
            found_again.was_hit(),
            "positive results outlive the negative TTL"
        );
        assert_eq!(found_again.into_value(), Some(found));

        let missing_again = cache
            .get_or_try_insert_uuid("missing".to_owned(), load(Some(found)))
            .await
            .expect("cache reload should succeed");
        assert!(
            !missing_again.was_hit(),
            "negative results expire after their TTL"
        );
        assert_eq!(missing_again.into_value(), Some(found));
    }

    #[tokio::test]
    async fn does_not_cache_failed_loads() {
        let cache = CacheManager::default();

        let error = cache
            .get_or_try_insert_uuid("player".to_owned(), async {
                Err::<Option<Uuid>, _>("down")
            })
            .await
            .expect_err("failed load should return its error");
        assert_eq!(*error, "down");

        let retried = cache
            .get_or_try_insert_uuid("player".to_owned(), async { Ok::<_, &str>(None) })
            .await
            .expect("retry should run the loader again");
        assert!(!retried.was_hit());
        assert_eq!(retried.into_value(), None);
    }

    #[tokio::test]
    async fn coalesces_concurrent_loads_for_the_same_key() {
        let cache = CacheManager::default();
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
        assert!(
            results.iter().all(|result| !result.was_hit()),
            "waiting for an in-flight load is a miss"
        );
        assert!(
            results
                .into_iter()
                .all(|result| result.into_value() == Some(Uuid::nil()))
        );

        let cached = cache
            .get_or_try_insert_uuid("player".to_owned(), load())
            .await
            .expect("cache lookup should succeed");
        assert!(cached.was_hit(), "a cached value is a hit");
        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn shares_cached_skin_strings() {
        let cache = CacheManager::default();
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
