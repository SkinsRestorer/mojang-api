package net.skinsrestorer.mojangapi;

import com.github.benmanes.caffeine.cache.Cache;
import com.github.benmanes.caffeine.cache.Caffeine;
import lombok.extern.slf4j.Slf4j;
import reactor.core.publisher.Mono;

import javax.annotation.Nullable;
import java.time.LocalDateTime;
import java.time.ZoneOffset;
import java.util.UUID;
import java.util.concurrent.TimeUnit;

@Slf4j
public class CacheManager implements AutoCloseable {
  private final Cache<String, DatabaseResult<UUID>> nameToUUIDCache = Caffeine.newBuilder()
    .expireAfterWrite(6, TimeUnit.HOURS)
    .build();
  private final Cache<UUID, DatabaseResult<SkinProperty>> uuidToSkinCache = Caffeine.newBuilder()
    .expireAfterWrite(6, TimeUnit.HOURS)
    .build();

  public void putNameToUUID(String name, @Nullable UUID uuid, LocalDateTime createdAt) {
    nameToUUIDCache.put(name, new DatabaseResult<>(createdAt.toEpochSecond(ZoneOffset.UTC), uuid));
  }

  public Mono<DatabaseResult<UUID>> getNameToUUID(String name) {
    return Mono.justOrEmpty(nameToUUIDCache.getIfPresent(name));
  }

  public void putUUIDToSkin(UUID uuid, @Nullable SkinProperty skinProperty, LocalDateTime createdAt) {
    uuidToSkinCache.put(uuid, new DatabaseResult<>(createdAt.toEpochSecond(ZoneOffset.UTC), skinProperty));
  }

  public Mono<DatabaseResult<SkinProperty>> getUUIDToSkin(UUID uuid) {
    return Mono.justOrEmpty(uuidToSkinCache.getIfPresent(uuid));
  }

  @Override
  public void close() {
    nameToUUIDCache.invalidateAll();
    uuidToSkinCache.invalidateAll();
    log.info("CacheManager closed and all caches invalidated.");
  }

  public record DatabaseResult<T>(long createdAt, @Nullable T value) {
  }

  public record SkinProperty(
    String value,
    String signature
  ) {
  }
}
