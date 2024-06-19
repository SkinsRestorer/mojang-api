package net.skinsrestorer.mojangapi;

import com.google.gson.Gson;
import com.linecorp.armeria.common.HttpMethod;
import com.linecorp.armeria.common.HttpResponse;
import com.linecorp.armeria.common.HttpStatus;
import com.linecorp.armeria.common.logging.LogLevel;
import com.linecorp.armeria.server.annotation.Decorator;
import com.linecorp.armeria.server.annotation.Get;
import com.linecorp.armeria.server.annotation.Param;
import com.linecorp.armeria.server.annotation.decorator.CorsDecorator;
import com.linecorp.armeria.server.annotation.decorator.LoggingDecorator;
import io.netty.handler.codec.http.HttpHeaderNames;
import lombok.RequiredArgsConstructor;
import net.skinsrestorer.mojangapi.responses.MojangProfileResponse;
import net.skinsrestorer.mojangapi.responses.MojangUUIDResponse;
import reactor.core.publisher.Mono;
import reactor.netty.http.client.HttpClient;

import javax.annotation.Nullable;
import java.net.URI;
import java.time.Duration;
import java.time.LocalDateTime;
import java.time.ZoneOffset;
import java.util.Arrays;
import java.util.UUID;

@Decorator(ThrottlingDecorator.class)
@LoggingDecorator(
  requestLogLevel = LogLevel.INFO,
  successfulResponseLogLevel = LogLevel.INFO
)
@CorsDecorator(origins = "*", allowedRequestMethods = HttpMethod.GET, credentialsAllowed = true, allowAllRequestHeaders = true)
@RequiredArgsConstructor
public class MojangAPIProxyService {
  private static final String MOJANG_UUID_URL = "https://api.mojang.com/users/profiles/minecraft/%s";
  private static final String MOJANG_PROFILE_URL = "https://sessionserver.mojang.com/session/minecraft/profile/%s?unsigned=false";
  private static final Gson GSON = new Gson();
  private final DatabaseManager databaseManager;

  @Get("/uuid/{name}")
  public HttpResponse nameToUUID(@Param String name) {
    if (ValidationUtil.invalidMinecraftUsername(name)) {
      return HttpResponse.ofJson(HttpStatus.BAD_REQUEST, new ErrorResponse(ErrorResponse.ErrorType.INVALID_NAME));
    }

    return HttpResponse.of(
      databaseManager.getNameToUUID(name)
        .map(cacheData -> HttpResponse.ofJson(HttpStatus.OK, new UUIDResponse(
          new CacheData(CacheState.HIT, cacheData.createdAt()),
          cacheData.value() != null,
          cacheData.value())))
        .switchIfEmpty(crawlMojangUUID(name))
        .onErrorResume(e -> Mono.just(HttpResponse.ofJson(HttpStatus.INTERNAL_SERVER_ERROR, new ErrorResponse(ErrorResponse.ErrorType.INTERNAL_ERROR))))
        .toFuture());
  }

  private Mono<HttpResponse> crawlMojangUUID(String name) {
    return HttpClient.create()
      .bindAddress(LocalAddressProvider::getRandomLocalAddress)
      .responseTimeout(Duration.ofSeconds(5))
      .compress(true)
      .headers(
        h -> {
          h.set(HttpHeaderNames.ACCEPT, "application/json");
          h.set(HttpHeaderNames.ACCEPT_LANGUAGE, "en-US,en");
          h.set(HttpHeaderNames.USER_AGENT, "SRMojangAPI");
        })
      .get()
      .uri(URI.create(String.format(MOJANG_UUID_URL, name)))
      .responseSingle((res, content) -> content.asString().map(responseText -> {
        var response = GSON.fromJson(responseText, MojangUUIDResponse.class);
        var uuid = response.getId() == null ? null : UUIDUtils.convertToDashed(response.getId());

        var time = LocalDateTime.now();
        databaseManager.putNameToUUID(name, uuid, time);

        return HttpResponse.ofJson(HttpStatus.OK, new UUIDResponse(new CacheData(CacheState.MISS, time.toEpochSecond(ZoneOffset.UTC)), uuid != null, uuid));
      }));
  }

  @Get("/skin/{uuid}")
  public HttpResponse uuidToSkin(@Param String uuid) {
    return UUIDUtils.tryParseUniqueId(uuid).map(value ->
        HttpResponse.of(databaseManager.getUUIDToSkin(value)
          .map(cacheData -> HttpResponse.ofJson(HttpStatus.OK, new ProfileResponse(
            new CacheData(CacheState.HIT, cacheData.createdAt()),
            cacheData.value() != null,
            cacheData.value() != null ? new ProfileResponse.SkinProperty(
              cacheData.value().value(),
              cacheData.value().signature()
            ) : null)))
          .switchIfEmpty(crawlMojangProfile(value))
          .onErrorResume(e -> Mono.just(HttpResponse.ofJson(HttpStatus.INTERNAL_SERVER_ERROR, new ErrorResponse(ErrorResponse.ErrorType.INTERNAL_ERROR))))
          .toFuture()
        ))
      .orElseGet(() -> HttpResponse.ofJson(HttpStatus.BAD_REQUEST, new ErrorResponse(ErrorResponse.ErrorType.INVALID_UUID)));
  }

  private Mono<HttpResponse> crawlMojangProfile(UUID uuid) {
    return HttpClient.create()
      .bindAddress(LocalAddressProvider::getRandomLocalAddress)
      .responseTimeout(Duration.ofSeconds(5))
      .compress(true)
      .headers(
        h -> {
          h.set(HttpHeaderNames.ACCEPT, "application/json");
          h.set(HttpHeaderNames.ACCEPT_LANGUAGE, "en-US,en");
          h.set(HttpHeaderNames.USER_AGENT, "SRMojangAPI");
        })
      .get()
      .uri(URI.create(String.format(MOJANG_PROFILE_URL, UUIDUtils.convertToNoDashes(uuid))))
      .responseSingle((res, content) -> {
        if (res.status().code() == 204) {
          var time = LocalDateTime.now();
          databaseManager.putUUIDToSkin(uuid, null, time);

          return Mono.just(HttpResponse.ofJson(HttpStatus.OK, new ProfileResponse(new CacheData(CacheState.MISS, time.toEpochSecond(ZoneOffset.UTC)), false, null)));
        }

        return content.asString().map(responseText -> {
          var response = GSON.fromJson(responseText, MojangProfileResponse.class);
          var property = response.getProperties() == null ? null : Arrays.stream(response.getProperties())
            .filter(p -> "textures".equals(p.getName()))
            .findFirst()
            .orElse(null);

          var time = LocalDateTime.now();
          databaseManager.putUUIDToSkin(uuid, property == null ? null
            : new DatabaseManager.SkinProperty(property.getValue(), property.getSignature()), time);

          return HttpResponse.ofJson(HttpStatus.OK, new ProfileResponse(new CacheData(CacheState.MISS, time.toEpochSecond(ZoneOffset.UTC)), property != null, property != null ? new ProfileResponse.SkinProperty(
            property.getValue(),
            property.getSignature()
          ) : null));
        });
      });
  }

  public enum CacheState {
    HIT,
    MISS
  }

  public record ErrorResponse(ErrorType error) {
    public enum ErrorType {
      INVALID_NAME,
      INVALID_UUID,
      INTERNAL_ERROR
    }
  }

  public record UUIDResponse(CacheData cacheData, boolean exists, @Nullable UUID uuid) {
  }

  public record ProfileResponse(CacheData cacheData, boolean exists, @Nullable SkinProperty skinProperty) {
    public record SkinProperty(
      String value,
      String signature
    ) {
    }
  }

  public record CacheData(
    CacheState state,
    long createdAt
  ) {
  }
}
