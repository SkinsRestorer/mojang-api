package net.skinsrestorer.mojangapi;

import com.linecorp.armeria.common.HttpMethod;
import com.linecorp.armeria.common.HttpResponse;
import com.linecorp.armeria.common.HttpStatus;
import com.linecorp.armeria.common.logging.LogLevel;
import com.linecorp.armeria.server.annotation.Get;
import com.linecorp.armeria.server.annotation.Param;
import com.linecorp.armeria.server.annotation.decorator.CorsDecorator;
import com.linecorp.armeria.server.annotation.decorator.LoggingDecorator;

import java.util.Optional;
import java.util.UUID;

@LoggingDecorator(
  requestLogLevel = LogLevel.INFO,
  successfulResponseLogLevel = LogLevel.INFO
)
@CorsDecorator(origins = "*", allowedRequestMethods = HttpMethod.GET, credentialsAllowed = true, allowAllRequestHeaders = true)
public class MojangAPIProxyService {
  @Get("/uuid/{name}")
  public HttpResponse nameToUUID(@Param String name) {
    if (ValidationUtil.invalidMinecraftUsername(name)) {
      return HttpResponse.ofJson(HttpStatus.BAD_REQUEST, new ErrorResponse(ErrorResponse.ErrorType.INVALID_NAME));
    }

    return HttpResponse.ofJson(HttpStatus.OK, new UUIDResponse(new CacheData(CacheState.MISS, 0), UUID.randomUUID()));
  }

  @Get("/profile/{uuid}")
  public HttpResponse uuidToProfile(@Param String uuid) {
    Optional<UUID> optionalUUID = UUIDUtils.tryParseUniqueId(uuid);
    if (optionalUUID.isEmpty()) {
      return HttpResponse.ofJson(HttpStatus.BAD_REQUEST, new ErrorResponse(ErrorResponse.ErrorType.INVALID_UUID));
    }

    return HttpResponse.ofJson(HttpStatus.OK, new ProfileResponse(new CacheData(CacheState.MISS, 0), new ProfileResponse.SkinProperty("value", "signature")));
  }

  public enum CacheState {
    HIT,
    MISS
  }

  public record ErrorResponse(ErrorType error) {
    public enum ErrorType {
      INVALID_NAME,
      INVALID_UUID
    }
  }

  public record UUIDResponse(CacheData cacheData, UUID uuid) {
  }

  public record ProfileResponse(CacheData cacheData, SkinProperty skinProperty) {
    public record SkinProperty(
      String value,
      String signature
    ) {
    }
  }

  public record CacheData(
    CacheState state,
    long expirationTime
  ) {
  }
}
