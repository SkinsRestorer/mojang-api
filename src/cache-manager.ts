import { LRUCache } from "lru-cache";

export interface DatabaseResult<T> {
  createdAt: number;
  value: T | null;
}

export interface SkinProperty {
  value: string;
  signature: string;
}

export interface CacheManager {
  putNameToUUID: (name: string, uuid: string | null, createdAt: Date) => void;
  getNameToUUID: (name: string) => Promise<DatabaseResult<string> | undefined>;
  putUUIDToSkin: (
    uuid: string,
    skinProperty: SkinProperty | null,
    createdAt: Date,
  ) => void;
  getUUIDToSkin: (
    uuid: string,
  ) => Promise<DatabaseResult<SkinProperty> | undefined>;
  close: () => void;
}

export function createCacheManager(): CacheManager {
  // Create caches with similar settings to the Java implementation
  const nameToUUIDCache = new LRUCache<string, DatabaseResult<string>>({
    max: 10000, // Maximum size of 10,000 entries
    ttl: 1000 * 60 * 60 * 6, // 6 hours (matching Java implementation)
  });

  const uuidToSkinCache = new LRUCache<string, DatabaseResult<SkinProperty>>({
    max: 10000, // Maximum size of 10,000 entries
    ttl: 1000 * 60 * 60 * 6, // 6 hours (matching Java implementation)
  });

  return {
    putNameToUUID(name: string, uuid: string | null, createdAt: Date) {
      nameToUUIDCache.set(name.toLowerCase(), {
        createdAt: Math.floor(createdAt.getTime() / 1000),
        value: uuid,
      });
    },

    async getNameToUUID(
      name: string,
    ): Promise<DatabaseResult<string> | undefined> {
      return nameToUUIDCache.get(name.toLowerCase());
    },

    putUUIDToSkin(
      uuid: string,
      skinProperty: SkinProperty | null,
      createdAt: Date,
    ) {
      uuidToSkinCache.set(uuid.toLowerCase(), {
        createdAt: Math.floor(createdAt.getTime() / 1000),
        value: skinProperty,
      });
    },

    async getUUIDToSkin(
      uuid: string,
    ): Promise<DatabaseResult<SkinProperty> | undefined> {
      return uuidToSkinCache.get(uuid.toLowerCase());
    },

    close() {
      nameToUUIDCache.clear();
      uuidToSkinCache.clear();
      console.log("CacheManager closed and all caches invalidated.");
    },
  };
}
