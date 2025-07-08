package net.skinsrestorer.mojangapi;

import com.google.gson.Gson;
import com.linecorp.armeria.common.*;
import com.linecorp.armeria.common.logging.LogLevel;
import com.linecorp.armeria.server.annotation.Decorator;
import com.linecorp.armeria.server.annotation.Get;
import com.linecorp.armeria.server.annotation.Param;
import com.linecorp.armeria.server.annotation.decorator.CorsDecorator;
import com.linecorp.armeria.server.annotation.decorator.LoggingDecorator;
import io.netty.handler.codec.http.HttpHeaderNames;
import lombok.RequiredArgsConstructor;
import lombok.extern.slf4j.Slf4j;
import net.skinsrestorer.mojangapi.responses.MojangProfileResponse;
import net.skinsrestorer.mojangapi.responses.MojangUUIDResponse;
import reactor.core.publisher.Mono;
import reactor.netty.http.client.HttpClient;
import reactor.netty.resources.ConnectionProvider;

import javax.annotation.Nullable;
import java.net.URI;
import java.time.Duration;
import java.time.LocalDateTime;
import java.util.Arrays;
import java.util.UUID;

@Slf4j
@Decorator(ThrottlingDecorator.class)
@LoggingDecorator(
  requestLogLevel = LogLevel.INFO,
  successfulResponseLogLevel = LogLevel.INFO
)
@CorsDecorator(origins = "*", allowedRequestMethods = HttpMethod.GET, credentialsAllowed = true, allowAllRequestHeaders = true)
@RequiredArgsConstructor
public class MojangAPIProxyService {
  private static final HttpClient HTTP_CLIENT = HttpClient.create(ConnectionProvider.builder("mojang-api")
      .maxConnections(50)
      .maxIdleTime(Duration.ofSeconds(20))
      .maxLifeTime(Duration.ofSeconds(60))
      .pendingAcquireTimeout(Duration.ofSeconds(60))
      .evictInBackground(Duration.ofSeconds(120))
      .disposeInactivePoolsInBackground(Duration.ofSeconds(120), Duration.ofSeconds(120))
      .build())
    .bindAddress(LocalAddressProvider::getRandomLocalAddress)
    .responseTimeout(Duration.ofSeconds(5))
    .compress(true)
    .headers(
      h -> {
        h.set(HttpHeaderNames.ACCEPT, "application/json");
        h.set(HttpHeaderNames.ACCEPT_LANGUAGE, "en-US,en");
        h.set(HttpHeaderNames.USER_AGENT, "SRMojangAPI");
      });
  private static final String MOJANG_UUID_URL = "https://api.minecraftservices.com/minecraft/profile/lookup/name/%s";
  private static final String MOJANG_PROFILE_URL = "https://sessionserver.mojang.com/session/minecraft/profile/%s?unsigned=false";
  private static final Gson GSON = new Gson();
  private final CacheManager cacheManager;
  public static final Duration CACHE_DURATION = Duration.ofMinutes(15);
  public static final ResponseHeaders CACHE_HEADERS = ResponseHeaders.builder(HttpStatus.OK)
    .add(HttpHeaderNames.CACHE_CONTROL, ServerCacheControl.builder()
      .cachePublic()
      .maxAge(CACHE_DURATION)
      .build()
      .asHeaderValue())
    .contentType(MediaType.JSON)
    .build();
  private static final HttpResponse INVALID_NAME_RESPONSE = HttpResponse.ofJson(HttpStatus.BAD_REQUEST, new ErrorResponse(ErrorResponse.ErrorType.INVALID_NAME));
  private static final HttpResponse INVALID_UUID_RESPONSE = HttpResponse.ofJson(HttpStatus.BAD_REQUEST, new ErrorResponse(ErrorResponse.ErrorType.INVALID_UUID));
  private static final HttpResponse INTERNAL_ERROR_RESPONSE = HttpResponse.ofJson(HttpStatus.INTERNAL_SERVER_ERROR, new ErrorResponse(ErrorResponse.ErrorType.INTERNAL_ERROR));
  private static final HttpResponse INTERNAL_TIMEOUT_RESPONSE = HttpResponse.ofJson(HttpStatus.SERVICE_UNAVAILABLE, new ErrorResponse(ErrorResponse.ErrorType.INTERNAL_TIMEOUT));

  @Get("/uuid/{name}")
  public HttpResponse nameToUUID(@Param String name) {
    if (ValidationUtil.invalidMinecraftUsername(name)) {
      return INVALID_NAME_RESPONSE;
    }

    return HttpResponse.of(
      cacheManager.getNameToUUID(name)
        .map(cacheData -> HttpResponse.ofJson(
          CACHE_HEADERS,
          new UUIDResponse(cacheData.value() != null, cacheData.value())
        ))
        .switchIfEmpty(crawlMojangUUID(name))
        .doOnError(e -> log.error("Failed to fetch UUID for name {}", name, e))
        .onErrorResume(e -> Mono.just(INTERNAL_ERROR_RESPONSE))
        .timeout(Duration.ofSeconds(10), Mono.just(INTERNAL_TIMEOUT_RESPONSE))
        .toFuture());
  }

  private Mono<HttpResponse> crawlMojangUUID(String name) {
    return HTTP_CLIENT
      .get()
      .uri(URI.create(String.format(MOJANG_UUID_URL, name)))
      .responseSingle((res, content) -> content.asString().map(responseText -> {
        boolean isNotFound = res.status().code() == 404;
        if (!isNotFound && res.status().codeClass() != io.netty.handler.codec.http.HttpStatusClass.SUCCESS) {
          return INTERNAL_ERROR_RESPONSE;
        }

        var response = GSON.fromJson(responseText, MojangUUIDResponse.class);
        var uuid = isNotFound || response.getId() == null ? null : UUIDUtils.convertToDashed(response.getId());

        var time = LocalDateTime.now();
        cacheManager.putNameToUUID(name, uuid, time);

        return HttpResponse.ofJson(
          CACHE_HEADERS,
          new UUIDResponse(uuid != null, uuid)
        );
      }));
  }

  @Get("/skin/{uuid}")
  public HttpResponse uuidToSkin(@Param String uuid) {
    return UUIDUtils.tryParseUniqueId(uuid).map(value ->
        HttpResponse.of(cacheManager.getUUIDToSkin(value)
          .map(cacheData -> HttpResponse.ofJson(
            CACHE_HEADERS,
            new ProfileResponse(
              cacheData.value() != null,
              cacheData.value() != null ? new ProfileResponse.SkinProperty(
                cacheData.value().value(),
                cacheData.value().signature()
              ) : null)
          ))
          .switchIfEmpty(crawlMojangProfile(value))
          .doOnError(e -> log.error("Failed to fetch skin for UUID {}", value, e))
          .onErrorResume(e -> Mono.just(INTERNAL_ERROR_RESPONSE))
          .timeout(Duration.ofSeconds(10), Mono.just(INTERNAL_TIMEOUT_RESPONSE))
          .toFuture()
        ))
      .orElse(INVALID_UUID_RESPONSE);
  }

  private Mono<HttpResponse> crawlMojangProfile(UUID uuid) {
    return HTTP_CLIENT
      .get()
      .uri(URI.create(String.format(MOJANG_PROFILE_URL, UUIDUtils.convertToNoDashes(uuid))))
      .responseSingle((res, content) -> {
        if (res.status().codeClass() != io.netty.handler.codec.http.HttpStatusClass.SUCCESS) {
          return Mono.just(INTERNAL_ERROR_RESPONSE);
        }

        if (res.status().code() == 204) {
          var time = LocalDateTime.now();
          cacheManager.putUUIDToSkin(uuid, null, time);

          return Mono.just(HttpResponse.ofJson(
            CACHE_HEADERS,
            new ProfileResponse(false, null)
          ));
        }

        return content.asString().map(responseText -> {
          var response = GSON.fromJson(responseText, MojangProfileResponse.class);
          var property = response.getProperties() == null ? null : Arrays.stream(response.getProperties())
            .filter(p -> "textures".equals(p.getName()))
            .findFirst()
            .orElse(null);

          var time = LocalDateTime.now();
          cacheManager.putUUIDToSkin(uuid, property == null ? null
            : new CacheManager.SkinProperty(property.getValue(), property.getSignature()), time);

          return HttpResponse.ofJson(
            CACHE_HEADERS,
            new ProfileResponse(property != null, property != null ? new ProfileResponse.SkinProperty(
              property.getValue(),
              property.getSignature()
            ) : null)
          );
        });
      });
  }

  public record ErrorResponse(ErrorType error) {
    public enum ErrorType {
      INVALID_NAME,
      INVALID_UUID,
      INTERNAL_TIMEOUT,
      INTERNAL_ERROR
    }
  }

  public record UUIDResponse(boolean exists, @Nullable UUID uuid) {
  }

  public record ProfileResponse(boolean exists, @Nullable SkinProperty skinProperty) {
    public record SkinProperty(
      String value,
      String signature
    ) {
    }
  }
}
