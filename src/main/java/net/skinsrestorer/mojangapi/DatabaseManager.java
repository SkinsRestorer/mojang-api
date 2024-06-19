package net.skinsrestorer.mojangapi;

import io.r2dbc.postgresql.codec.PostgresqlObjectId;
import io.r2dbc.spi.*;
import lombok.Getter;
import lombok.extern.slf4j.Slf4j;
import reactor.core.publisher.Mono;

import javax.annotation.Nullable;
import java.sql.Timestamp;
import java.time.Duration;
import java.util.HashMap;
import java.util.Objects;
import java.util.UUID;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;

import static io.r2dbc.postgresql.PostgresqlConnectionFactoryProvider.APPLICATION_NAME;
import static io.r2dbc.postgresql.PostgresqlConnectionFactoryProvider.OPTIONS;
import static io.r2dbc.spi.ConnectionFactoryOptions.*;

@Slf4j
public class DatabaseManager implements AutoCloseable {
  public static final Duration UUID_CACHE_DURATION = Duration.ofDays(1);
  public static final Duration SKIN_CACHE_DURATION = Duration.ofDays(1);
  public static final int UUID_MAX_ROWS = 10_000;
  public static final int SKIN_MAX_ROWS = 10_000;
  @Getter
  private final ConnectionFactory connectionFactory;
  private final ScheduledExecutorService cleanupExecutor = Executors.newSingleThreadScheduledExecutor();

  public DatabaseManager() {
    this.connectionFactory = ConnectionFactories.get(ConnectionFactoryOptions.builder()
      .option(DRIVER, "pool")
      .option(PROTOCOL, "postgresql")
      .option(APPLICATION_NAME, "mojang-api")
      .option(HOST, System.getenv("JDBC_DATABASE_HOST"))
      .option(PORT, Integer.parseInt(System.getenv("JDBC_DATABASE_PORT")))
      .option(DATABASE, System.getenv("JDBC_DATABASE_NAME"))
      .option(USER, System.getenv("JDBC_DATABASE_USERNAME"))
      .option(PASSWORD, System.getenv("JDBC_DATABASE_PASSWORD"))
      .option(LOCK_WAIT_TIMEOUT, Duration.ofSeconds(15))
      .option(STATEMENT_TIMEOUT, Duration.ofSeconds(15))
      .build());

    createTables();
    scheduleCleanup();
  }

  private Mono<? extends Connection> createConnection() {
    return Mono.from(connectionFactory.create());
  }

  public void createTables() {
    var conn = createConnection();
    conn.flatMapMany(it -> it.createStatement(
          "CREATE TABLE IF NOT EXISTS uuid_cache ("
            + "name VARCHAR(16) PRIMARY KEY,"
            + "uuid VARCHAR(36),"
            + "created_at TIMESTAMP NOT NULL"
            + ")")
        .execute())
      .blockLast();

    conn.flatMapMany(it -> it.createStatement(
          "CREATE TABLE IF NOT EXISTS skin_cache ("
            + "uuid VARCHAR(36) PRIMARY KEY,"
            + "value TEXT,"
            + "signature TEXT,"
            + "created_at TIMESTAMP NOT NULL"
            + ")")
        .execute())
      .blockLast();

    // Delete old rows if the table exceeds the maximum number of rows
    conn.flatMapMany(it -> it.createStatement(
          "CREATE OR REPLACE FUNCTION delete_old_uuid_cache_rows() RETURNS TRIGGER AS $$"
            + "BEGIN"
            + " IF (SELECT COUNT(*) FROM uuid_cache) > " + UUID_MAX_ROWS + " THEN"
            + " DELETE FROM uuid_cache WHERE created_at = (SELECT MIN(created_at) FROM uuid_cache);"
            + " END IF;"
            + " RETURN NEW;"
            + "END;"
            + "$$ LANGUAGE plpgsql")
        .execute())
      .blockLast();

    if (!triggerExists("delete_old_uuid_cache_rows_trigger")) {
      conn.flatMapMany(it -> it.createStatement(
            "CREATE TRIGGER delete_old_uuid_cache_rows_trigger"
              + " AFTER INSERT ON uuid_cache"
              + " EXECUTE FUNCTION delete_old_uuid_cache_rows()")
          .execute())
        .blockLast();
    }

    // Delete old rows if the table exceeds the maximum number of rows
    conn.flatMapMany(it -> it.createStatement(
          "CREATE OR REPLACE FUNCTION delete_old_skin_cache_rows() RETURNS TRIGGER AS $$"
            + "BEGIN"
            + " IF (SELECT COUNT(*) FROM skin_cache) > " + SKIN_MAX_ROWS + " THEN"
            + " DELETE FROM skin_cache WHERE created_at = (SELECT MIN(created_at) FROM skin_cache);"
            + " END IF;"
            + " RETURN NEW;"
            + "END;"
            + "$$ LANGUAGE plpgsql")
        .execute())
      .blockLast();

    if (!triggerExists("delete_old_skin_cache_rows_trigger")) {
      conn.flatMapMany(it -> it.createStatement(
            "CREATE TRIGGER delete_old_skin_cache_rows_trigger"
              + " AFTER INSERT ON skin_cache"
              + " EXECUTE FUNCTION delete_old_skin_cache_rows()")
          .execute())
        .blockLast();
    }
  }

  private boolean triggerExists(String triggerName) {
    return Boolean.TRUE.equals(createConnection()
      .flatMapMany(it -> it.createStatement("SELECT 1 FROM pg_trigger WHERE tgname = $1")
        .bind("$1", triggerName)
        .execute())
      .map(it -> true)
      .defaultIfEmpty(false)
      .blockLast());
  }

  public void scheduleCleanup() {
    cleanupExecutor.scheduleWithFixedDelay(() -> {
      createConnection()
        .flatMapMany(it -> it.createStatement("DELETE FROM uuid_cache WHERE created_at < NOW() - INTERVAL '" + UUID_CACHE_DURATION.toHours() + "' HOUR")
          .execute())
        .flatMap(Result::getRowsUpdated)
        .doOnNext(it -> log.info("Cleaned up {} rows from uuid_cache", it))
        .subscribe();

      createConnection()
        .flatMapMany(it -> it.createStatement("DELETE FROM skin_cache WHERE created_at < NOW() - INTERVAL '" + SKIN_CACHE_DURATION.toHours() + "' HOUR")
          .execute())
        .flatMap(Result::getRowsUpdated)
        .doOnNext(it -> log.info("Cleaned up {} rows from skin_cache", it))
        .subscribe();
    }, 0, 6, TimeUnit.HOURS);
  }

  public void putNameToUUID(String name, @Nullable UUID uuid, long createdAt) {
    createConnection()
      .flatMapMany(it -> it.createStatement("INSERT INTO uuid_cache (name, uuid, created_at) VALUES ($1, $2, $3) ON CONFLICT (name) DO UPDATE SET uuid = EXCLUDED.uuid")
        .bind("$1", name)
        .bind("$2", uuid == null ? null : uuid.toString())
        .bind("$3", new Timestamp(createdAt))
        .execute())
      .flatMap(Result::getRowsUpdated)
      .doOnNext(it -> log.debug("Inserted {} rows into uuid_cache", it))
      .subscribe();
  }

  public Mono<DatabaseResult<UUID>> getNameToUUID(String name) {
    return createConnection()
      .flatMapMany(it -> it.createStatement("SELECT uuid, created_at FROM uuid_cache WHERE name = $1")
        .bind("$1", name)
        .execute())
      .next()
      .flatMapMany(it -> it.map((row, meta) -> {
        var createdAt = Objects.requireNonNull(row.get("created_at", Timestamp.class)).getTime();
        var uuid = row.get("uuid", String.class);

        return new DatabaseResult<>(createdAt, uuid == null ? null : UUID.fromString(uuid));
      }))
      .next();
  }

  public void putUUIDToSkin(UUID uuid, @Nullable SkinProperty skinProperty, long createdAt) {
    createConnection()
      .flatMapMany(it -> it.createStatement("INSERT INTO skin_cache (uuid, value, signature, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT (uuid) DO UPDATE SET value = EXCLUDED.value, signature = EXCLUDED.signature")
        .bind("$1", uuid.toString())
        .bind("$2", Parameters.in(PostgresqlObjectId.TEXT, skinProperty == null ? null : skinProperty.value()))
        .bind("$3", Parameters.in(PostgresqlObjectId.TEXT, skinProperty == null ? null : skinProperty.signature()))
        .bind("$4", new Timestamp(createdAt))
        .execute())
      .flatMap(Result::getRowsUpdated)
      .doOnNext(it -> log.debug("Inserted {} rows into skin_cache", it))
      .subscribe();
  }

  public Mono<DatabaseResult<SkinProperty>> getUUIDToSkin(UUID uuid) {
    return createConnection()
      .flatMapMany(it -> it.createStatement("SELECT value, signature, created_at FROM skin_cache WHERE uuid = $1")
        .bind("$1", uuid.toString())
        .execute())
      .next()
      .flatMapMany(it -> it.map((row, meta) -> {
        var createdAt = Objects.requireNonNull(row.get("created_at", Timestamp.class)).getTime();
        var value = row.get("value", String.class);
        var signature = row.get("signature", String.class);

        return new DatabaseResult<>(createdAt, value == null || signature == null ? null : new SkinProperty(value, signature));
      }))
      .next();
  }

  @Override
  public void close() {
    cleanupExecutor.shutdown();
  }

  public record DatabaseResult<T>(long createdAt, @Nullable T value) {
  }

  public record SkinProperty(
    String value,
    String signature
  ) {
  }
}
