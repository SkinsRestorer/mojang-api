package net.skinsrestorer.mojangapi;

import com.linecorp.armeria.common.HttpRequest;
import com.linecorp.armeria.common.HttpResponse;
import com.linecorp.armeria.server.DecoratingHttpServiceFunction;
import com.linecorp.armeria.server.HttpService;
import com.linecorp.armeria.server.ServiceRequestContext;
import com.linecorp.armeria.server.throttling.ThrottlingService;
import com.linecorp.armeria.server.throttling.ThrottlingStrategy;
import reactor.util.annotation.NonNull;

public class ThrottlingDecorator implements DecoratingHttpServiceFunction {
  @Override
  public @NonNull HttpResponse serve(HttpService delegate, @NonNull ServiceRequestContext ctx, @NonNull HttpRequest req) throws Exception {
    return delegate.decorate(ThrottlingService.newDecorator(
        ThrottlingStrategy.rateLimiting(1000.0)))
      .serve(ctx, req);
  }
}
