package net.skinsrestorer.mojangapi;

import com.linecorp.armeria.common.HttpHeaderNames;
import com.linecorp.armeria.server.ClientAddressSource;
import com.linecorp.armeria.server.RedirectService;
import com.linecorp.armeria.server.Server;
import com.linecorp.armeria.server.docs.DocService;
import com.linecorp.armeria.server.healthcheck.HealthCheckService;
import lombok.extern.slf4j.Slf4j;

import java.time.Duration;

@Slf4j
public class Main {
  public static void main(String[] args) {
    try (var databaseManager = new DatabaseManager()) {
      final Server server =
        Server.builder()
          .http(Integer.parseInt(System.getenv("SERVER_PORT")))
          .maxRequestLength(5 * 1024)  // 5 kB
          .requestTimeout(Duration.ofSeconds(10))
          .clientAddressTrustedProxyFilter(a -> true)
          .clientAddressSources(ClientAddressSource.ofHeader(HttpHeaderNames.X_FORWARDED_FOR))
          .annotatedService("/mojang", new MojangAPIProxyService(databaseManager))
          .service("/health", HealthCheckService.builder().build())
          .service("/", new RedirectService("/docs"))
          .serviceUnder("/docs",
            DocService.builder()
              .examplePaths(MojangAPIProxyService.class,
                "nameToUUID",
                "/mojang/uuid/Pistonmaster")
              .examplePaths(MojangAPIProxyService.class,
                "uuidToSkin",
                "/mojang/skin/b1ae0778-4817-436c-96a3-a72c67cda060")
              .build())
          .build();

      server.closeOnJvmShutdown();

      server.start().join();

      log.info("Server has been started. Serving DocService at http://127.0.0.1:{}/docs",
        server.activeLocalPort());

      server.blockUntilShutdown();
    } catch (Exception e) {
      throw new RuntimeException(e);
    }
  }
}
