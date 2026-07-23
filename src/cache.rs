use std::time::Duration;

use moka::future::Cache;
use uuid::Uuid;

use crate::types::SkinProperty;

const CACHE_CAPACITY: u64 = 10_000;
const CACHE_TTL: Duration = Duration::from_hours(6);

#[derive(Debug, Clone)]
pub struct CacheManager {
    names: Cache<String, Option<Uuid>>,
    skins: Cache<Uuid, Option<SkinProperty>>,
}

impl Default for CacheManager {
    fn default() -> Self {
        Self::new(CACHE_CAPACITY, CACHE_TTL)
    }
}

impl CacheManager {
    #[must_use]
    pub fn new(capacity: u64, ttl: Duration) -> Self {
        Self {
            names: Cache::builder()
                .max_capacity(capacity)
                .time_to_live(ttl)
                .build(),
            skins: Cache::builder()
                .max_capacity(capacity)
                .time_to_live(ttl)
                .build(),
        }
    }

    pub async fn get_uuid(&self, name: &str) -> Option<Option<Uuid>> {
        self.names.get(&name.to_ascii_lowercase()).await
    }

    pub async fn put_uuid(&self, name: &str, uuid: Option<Uuid>) {
        self.names.insert(name.to_ascii_lowercase(), uuid).await;
    }

    pub async fn get_skin(&self, uuid: Uuid) -> Option<Option<SkinProperty>> {
        self.skins.get(&uuid).await
    }

    pub async fn put_skin(&self, uuid: Uuid, property: Option<SkinProperty>) {
        self.skins.insert(uuid, property).await;
    }

    pub async fn clear(&self) {
        self.names.invalidate_all();
        self.skins.invalidate_all();
        self.names.run_pending_tasks().await;
        self.skins.run_pending_tasks().await;
    }
}
