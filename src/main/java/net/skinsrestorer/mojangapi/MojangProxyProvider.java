package net.skinsrestorer.mojangapi;

import com.google.gson.Gson;
import io.netty.handler.codec.http.HttpHeaderNames;
import lombok.extern.slf4j.Slf4j;
import reactor.netty.http.client.HttpClient;
import reactor.netty.transport.ProxyProvider;

import java.net.URI;
import java.security.SecureRandom;
import java.time.Duration;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Consumer;
import java.util.function.Supplier;

@Slf4j
public class MojangProxyProvider implements Supplier<Consumer<ProxyProvider.TypeSpec>>, AutoCloseable {
  private static final URI PROXY_ENDPOINT = URI.create(System.getenv("PROXY_ENDPOINT"));
  private static final Gson GSON = new Gson();
  private final ScheduledExecutorService crawlExecutor = Executors.newSingleThreadScheduledExecutor();
  private final SecureRandom random = new SecureRandom();
  private final AtomicReference<List<Consumer<ProxyProvider.TypeSpec>>> typeSpecs = new AtomicReference<>();

  public MojangProxyProvider() throws ExecutionException, InterruptedException {
    Runnable crawl = () -> {
      try {
        var typeSpecs = MojangProxyProvider.crawl();
        this.typeSpecs.set(typeSpecs);
      } catch (Exception e) {
        log.error("Failed to crawl proxy types", e);
      }
    };
    crawlExecutor.schedule(crawl, 0, TimeUnit.SECONDS).get();
    crawlExecutor.scheduleWithFixedDelay(crawl, 6, 6, TimeUnit.HOURS);
  }

  private static List<Consumer<ProxyProvider.TypeSpec>> crawl() {
    return HttpClient.create()
      .responseTimeout(Duration.ofSeconds(5))
      .headers(
        h -> {
          h.set(HttpHeaderNames.ACCEPT, "application/json");
          h.set(HttpHeaderNames.ACCEPT_LANGUAGE, "en-US,en");
          h.set(HttpHeaderNames.USER_AGENT, "SRMojangAPI");
        })
      .get()
      .uri(PROXY_ENDPOINT)
      .responseSingle(
        (res, content) ->
          content
            .asString()
            .map(
              responseText -> {
                var response = GSON.fromJson(responseText, ProxyResponse.class);

                var proxyList = new ArrayList<Consumer<ProxyProvider.TypeSpec>>();
                response.socks4.forEach(
                  host -> proxyList.add(createTypeSpec(host, ProxyType.SOCKS4)));
                response.socks5.forEach(
                    host -> proxyList.add(createTypeSpec(host, ProxyType.SOCKS5)));
                response.http.forEach(
                  host -> proxyList.add(createTypeSpec(host, ProxyType.HTTP)));
                response.https.forEach(
                  host -> proxyList.add(createTypeSpec(host, ProxyType.HTTPS)));

                return proxyList;
              }))
      .block();
  }

  @Override
  public Consumer<ProxyProvider.TypeSpec> get() {
    var typeSpecs = this.typeSpecs.get();
    return typeSpecs.get(random.nextInt(typeSpecs.size()));
  }

  @Override
  public void close() {
    crawlExecutor.shutdown();
  }

  private record ProxyResponse(List<String> socks4, List<String> socks5, List<String> http, List<String> https) {
  }

  private enum ProxyType {
    SOCKS4, SOCKS5, HTTP, HTTPS
  }

  private static Consumer<ProxyProvider.TypeSpec> createTypeSpec(String info, ProxyType type) {
    var split = info.split(":");
    var host = split[0];
    var port = Integer.parseInt(split[1]);

    return spec -> {
      spec.type(
        switch (type) {
          case HTTP, HTTPS -> ProxyProvider.Proxy.HTTP;
          case SOCKS4 -> ProxyProvider.Proxy.SOCKS4;
          case SOCKS5 -> ProxyProvider.Proxy.SOCKS5;
        })
        .host(host)
        .port(port)
        .nonProxyHosts("localhost")
        .connectTimeoutMillis(20_000);
    };
  }
}
