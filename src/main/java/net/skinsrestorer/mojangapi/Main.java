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
    final Server server = newServer(8080);

    server.closeOnJvmShutdown();

    server.start().join();

    logger.info("Server has been started. Serving DocService at http://127.0.0.1:{}/docs",
      server.activeLocalPort());
  }

  /**
   * Returns a new {@link Server} instance configured with annotated HTTP services.
   *
   * @param port the port that the server is to be bound to
   */
  @SuppressWarnings("SameParameterValue")
  private static Server newServer(int port) {
    final ServerBuilder sb = Server.builder();
    sb.http(port);
    configureServices(sb);
    return sb.build();
  }

  static void configureServices(ServerBuilder sb) {
    sb.annotatedService("/mojang", new MojangAPIProxyService())
      .service("/", new RedirectService("/docs"))
      .serviceUnder("/docs",
        DocService.builder()
          .examplePaths(MojangAPIProxyService.class,
            "nameToUUID",
            "/mojang/uuid/Pistonmaster")
          .examplePaths(MojangAPIProxyService.class,
            "uuidToProfile",
            "/mojang/profile/b1ae0778-4817-436c-96a3-a72c67cda060")
          .build());
  }
}
