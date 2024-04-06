package net.skinsrestorer.mojangapi;

import com.zaxxer.hikari.HikariConfig;
import com.zaxxer.hikari.HikariDataSource;
import lombok.Getter;

import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;

public class DatabaseManager implements AutoCloseable {
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
          + "uuid VARCHAR(36) NOT NULL,"
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
          + "value TEXT NOT NULL,"
          + "signature TEXT NOT NULL,"
          + "created_at TIMESTAMP NOT NULL"
          + ")");
    } catch (Exception e) {
      throw new RuntimeException(e);
    }
  }

  public void scheduleCleanup() {
    cleanupExecutor.scheduleWithFixedDelay(() -> {
      try (var conn = ds.getConnection();
           var stmt = conn.createStatement()) {
        stmt.execute("DELETE FROM uuid_cache WHERE created_at < NOW() - INTERVAL '1 day'");
        stmt.execute("DELETE FROM skin_cache WHERE created_at < NOW() - INTERVAL '1 day'");
      } catch (Exception e) {
        throw new RuntimeException(e);
      }
    }, 0, 1, TimeUnit.DAYS);
  }

  @Override
  public void close() throws Exception {
    ds.close();
  }
}
