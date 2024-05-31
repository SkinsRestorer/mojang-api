package net.skinsrestorer.mojangapi;

import com.zaxxer.hikari.HikariConfig;
import com.zaxxer.hikari.HikariDataSource;
import lombok.Getter;
import lombok.extern.slf4j.Slf4j;

import javax.annotation.Nullable;
import java.sql.Timestamp;
import java.time.Duration;
import java.util.UUID;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;

@Slf4j
public class DatabaseManager implements AutoCloseable {
  public static final Duration UUID_CACHE_DURATION = Duration.ofDays(1);
  public static final Duration SKIN_CACHE_DURATION = Duration.ofDays(1);
  public static final int UUID_MAX_ROWS = 10_000;
  public static final int SKIN_MAX_ROWS = 10_000;
  @Getter
  private final HikariDataSource ds;
  private final ScheduledExecutorService cleanupExecutor = Executors.newSingleThreadScheduledExecutor();

  public DatabaseManager() {
    var config = new HikariConfig();

    config.setJdbcUrl(System.getenv("JDBC_DATABASE_URL"));
    config.setUsername(System.getenv("JDBC_DATABASE_USERNAME"));
    config.setPassword(System.getenv("JDBC_DATABASE_PASSWORD"));
    config.addDataSourceProperty("cachePrepStmts", "true");
    config.addDataSourceProperty("prepStmtCacheSize", "250");
    config.addDataSourceProperty("prepStmtCacheSqlLimit", "2048");

    ds = new HikariDataSource(config);

    createTables();
    scheduleCleanup();
  }

  public void createTables() {
    try (var conn = ds.getConnection();
         var stmt = conn.createStatement()) {
      stmt.execute(
        "CREATE TABLE IF NOT EXISTS uuid_cache ("
          + "name VARCHAR(16) PRIMARY KEY,"
          + "uuid VARCHAR(36),"
          + "created_at TIMESTAMP NOT NULL"
          + ")");
      stmt.execute(
        "CREATE TABLE IF NOT EXISTS skin_cache ("
          + "uuid VARCHAR(36) PRIMARY KEY,"
          + "value TEXT,"
          + "signature TEXT,"
          + "created_at TIMESTAMP NOT NULL"
          + ")");

      // Delete old rows if the table exceeds the maximum number of rows
      stmt.execute(
        "CREATE OR REPLACE FUNCTION delete_old_uuid_cache_rows() RETURNS TRIGGER AS $$"
          + "BEGIN"
          + " IF (SELECT COUNT(*) FROM uuid_cache) > " + UUID_MAX_ROWS + " THEN"
          + " DELETE FROM uuid_cache WHERE created_at = (SELECT MIN(created_at) FROM uuid_cache);"
          + " END IF;"
          + " RETURN NEW;"
          + "END;"
          + "$$ LANGUAGE plpgsql");
      stmt.execute(
        "CREATE TRIGGER delete_old_uuid_cache_rows_trigger"
          + " AFTER INSERT ON uuid_cache"
          + " EXECUTE FUNCTION delete_old_uuid_cache_rows()");

      // Delete old rows if the table exceeds the maximum number of rows
      stmt.execute(
        "CREATE OR REPLACE FUNCTION delete_old_skin_cache_rows() RETURNS TRIGGER AS $$"
          + "BEGIN"
          + " IF (SELECT COUNT(*) FROM skin_cache) > " + SKIN_MAX_ROWS + " THEN"
          + " DELETE FROM skin_cache WHERE created_at = (SELECT MIN(created_at) FROM skin_cache);"
          + " END IF;"
          + " RETURN NEW;"
          + "END;"
          + "$$ LANGUAGE plpgsql");
      stmt.execute(
        "CREATE TRIGGER delete_old_skin_cache_rows_trigger"
          + " AFTER INSERT ON skin_cache"
          + " EXECUTE FUNCTION delete_old_skin_cache_rows()");
    } catch (Exception e) {
      throw new RuntimeException(e);
    }
  }

  public void scheduleCleanup() {
    cleanupExecutor.scheduleWithFixedDelay(() -> {
      try (var conn = ds.getConnection();
           var stmt = conn.prepareStatement("DELETE FROM uuid_cache WHERE created_at < NOW() - INTERVAL '" + UUID_CACHE_DURATION.toHours() + "' HOUR");
           var stmt2 = conn.prepareStatement("DELETE FROM skin_cache WHERE created_at < NOW() - INTERVAL '" + SKIN_CACHE_DURATION.toHours() + "' HOUR")) {
        var uuidRows = stmt.executeUpdate();
        var skinRows = stmt2.executeUpdate();
        log.info("Cleaned up {} rows from uuid_cache and {} rows from skin_cache", uuidRows, skinRows);
      } catch (Exception e) {
        throw new RuntimeException(e);
      }
    }, 0, 6, TimeUnit.HOURS);
  }

  public void putNameToUUID(String name, @Nullable UUID uuid, long createdAt) {
    try (var conn = ds.getConnection();
         var stmt = conn.prepareStatement("INSERT INTO uuid_cache (name, uuid, created_at) VALUES (?, ?, ?) ON CONFLICT (name) DO UPDATE SET uuid = EXCLUDED.uuid")) {
      stmt.setString(1, name);
      stmt.setString(2, uuid == null ? null : uuid.toString());
      stmt.setTimestamp(3, new Timestamp(createdAt));
      stmt.executeUpdate();
    } catch (Exception e) {
      throw new RuntimeException(e);
    }
  }

  public DatabaseResult<UUID> getNameToUUID(String name) {
    try (var conn = ds.getConnection();
         var stmt = conn.prepareStatement("SELECT uuid, created_at FROM uuid_cache WHERE name = ?")) {
      stmt.setString(1, name);

      try (var rs = stmt.executeQuery()) {
        if (rs.next()) {
          var createdAt = rs.getTimestamp(2).getTime();
          var uuid = rs.getString(1);

          return new DatabaseResult<>(createdAt, uuid == null ? null : UUID.fromString(uuid));
        }
      }
    } catch (Exception e) {
      throw new RuntimeException(e);
    }

    return null;
  }

  public void putUUIDToSkin(UUID uuid, @Nullable SkinProperty skinProperty, long createdAt) {
    try (var conn = ds.getConnection();
         var stmt = conn.prepareStatement("INSERT INTO skin_cache (uuid, value, signature, created_at) VALUES (?, ?, ?, ?) ON CONFLICT (uuid) DO UPDATE SET value = EXCLUDED.value, signature = EXCLUDED.signature")) {
      stmt.setString(1, uuid.toString());
      stmt.setString(2, skinProperty == null ? null : skinProperty.value());
      stmt.setString(3, skinProperty == null ? null : skinProperty.signature());
      stmt.setTimestamp(4, new Timestamp(createdAt));
      stmt.executeUpdate();
    } catch (Exception e) {
      throw new RuntimeException(e);
    }
  }

  public DatabaseResult<SkinProperty> getUUIDToSkin(UUID uuid) {
    try (var conn = ds.getConnection();
         var stmt = conn.prepareStatement("SELECT value, signature, created_at FROM skin_cache WHERE uuid = ?")) {
      stmt.setString(1, uuid.toString());

      try (var rs = stmt.executeQuery()) {
        if (rs.next()) {
          var createdAt = rs.getTimestamp(3).getTime();
          var value = rs.getString(1);
          var signature = rs.getString(2);

          return new DatabaseResult<>(createdAt, value == null || signature == null ? null : new SkinProperty(value, signature));
        }
      }
    } catch (Exception e) {
      throw new RuntimeException(e);
    }

    return null;
  }

  @Override
  public void close() {
    ds.close();
  }

  public record DatabaseResult<T>(long createdAt, @Nullable T value) {
  }

  public record SkinProperty(
    String value,
    String signature
  ) {
  }
}
