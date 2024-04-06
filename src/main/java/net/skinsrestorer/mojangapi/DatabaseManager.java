package net.skinsrestorer.mojangapi;

import com.zaxxer.hikari.HikariConfig;
import com.zaxxer.hikari.HikariDataSource;
import lombok.Getter;

import javax.annotation.Nullable;
import java.sql.Timestamp;
import java.time.Duration;
import java.util.UUID;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;

public class DatabaseManager implements AutoCloseable {
  public static final Duration UUID_CACHE_DURATION = Duration.ofDays(1);
  public static final Duration SKIN_CACHE_DURATION = Duration.ofDays(1);
  @Getter
  private final HikariDataSource ds;
  private final ScheduledExecutorService cleanupExecutor = Executors.newSingleThreadScheduledExecutor();

  public DatabaseManager() {
    // We use PostgreSQL
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
    } catch (Exception e) {
      throw new RuntimeException(e);
    }

    try (var conn = ds.getConnection();
         var stmt = conn.createStatement()) {
      stmt.execute(
        "CREATE TABLE IF NOT EXISTS skin_cache ("
          + "uuid VARCHAR(36) PRIMARY KEY,"
          + "value TEXT,"
          + "signature TEXT,"
          + "created_at TIMESTAMP NOT NULL"
          + ")");
    } catch (Exception e) {
      throw new RuntimeException(e);
    }
  }

  public void scheduleCleanup() {
    // cLEAN VIA VALUES FROm UUID_CACHE_DURATION AND SKIN_CACHE_DURATION
    cleanupExecutor.scheduleWithFixedDelay(() -> {
      try (var conn = ds.getConnection();
           var stmt = conn.prepareStatement("DELETE FROM uuid_cache WHERE created_at < NOW() - INTERVAL ? HOUR");
           var stmt2 = conn.prepareStatement("DELETE FROM skin_cache WHERE created_at < NOW() - INTERVAL ? HOUR")) {
        stmt.setInt(1, (int) UUID_CACHE_DURATION.toHours());
        stmt2.setInt(1, (int) SKIN_CACHE_DURATION.toHours());

        stmt.executeUpdate();
        stmt2.executeUpdate();
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
