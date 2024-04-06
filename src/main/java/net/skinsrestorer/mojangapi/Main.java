package net.skinsrestorer.mojangapi;

import com.linecorp.armeria.server.RedirectService;
import com.linecorp.armeria.server.Server;
import com.linecorp.armeria.server.ServerBuilder;
import com.linecorp.armeria.server.docs.DocService;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;

public class Main {

  private static final Logger logger = LoggerFactory.getLogger(Main.class);

  public static void main(String[] args) {
    try (var databaseManager = new DatabaseManager();
         var proxyProvider = new MojangProxyProvider()) {
      final Server server = newServer(8080, databaseManager, proxyProvider);

      server.closeOnJvmShutdown();

      server.start().join();

      logger.info("Server has been started. Serving DocService at http://127.0.0.1:{}/docs",
        server.activeLocalPort());

      server.blockUntilShutdown();
    } catch (Exception e) {
      throw new RuntimeException(e);
    }
  }

  /**
   * Returns a new {@link Server} instance configured with annotated HTTP services.
   *
   * @param port the port that the server is to be bound to
   */
  @SuppressWarnings("SameParameterValue")
  private static Server newServer(int port, DatabaseManager databaseManager, MojangProxyProvider proxyProvider) {
    final ServerBuilder sb = Server.builder();
    sb.http(port);
    configureServices(sb, databaseManager, proxyProvider);
    return sb.build();
  }

  static void configureServices(ServerBuilder sb, DatabaseManager databaseManager, MojangProxyProvider proxyProvider) {
    sb.annotatedService("/mojang", new MojangAPIProxyService(databaseManager, proxyProvider))
      .service("/", new RedirectService("/docs"))
      .serviceUnder("/docs",
        DocService.builder()
          .examplePaths(MojangAPIProxyService.class,
            "nameToUUID",
            "/mojang/uuid/Pistonmaster")
          .examplePaths(MojangAPIProxyService.class,
            "uuidToProfile",
            "/mojang/skin/b1ae0778-4817-436c-96a3-a72c67cda060")
          .build());
  }
}
