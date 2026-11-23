#![warn(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::{num::NonZeroU32, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, Request, State, rejection::PathRejection},
    http::{
        HeaderValue, Method, StatusCode,
        header::{CACHE_CONTROL, X_CONTENT_TYPE_OPTIONS},
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use ipnet::IpNet;
use serde::Serialize;
use tower_http::{
    cors::{Any, CorsLayer},
    trace::TraceLayer,
};
use utoipa::OpenApi;

use crate::{
    batch::BatchLookup,
    cache::{CacheLoad, CacheManager},
    docs,
    error::AppError,
    metrics::Metrics,
    mojang::MojangService,
    rate_limit::{RateLimiter, enforce_rate_limit},
    types::{
        ErrorResponse, HealthResponse, HealthStatus, SkinLookupResponse, SkinProperty,
        UuidLookupResponse,
    },
    validation::{is_valid_minecraft_username, parse_minecraft_uuid},
};

const RATE_LIMIT: NonZeroU32 = NonZeroU32::new(1_000).unwrap();
const RATE_LIMIT_WINDOW: Duration = Duration::from_mins(1);
/// A last-resort deadline. Lookups normally fail sooner through their own timeouts.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const FOUND_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("public, max-age=900");
/// Missing profiles may be created soon, so clients should not keep them for long.
const NOT_FOUND_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("public, max-age=60");

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BorrowedSkinLookupResponse<'a> {
    exists: bool,
    skin_property: Option<&'a SkinProperty>,
}

#[derive(Clone)]
pub struct AppState {
    pub batch_lookup: BatchLookup,
    pub cache: CacheManager,
    pub mojang: Arc<dyn MojangService>,
    pub metrics: Arc<Metrics>,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Mojang API Proxy",
        description = "Cached Minecraft username and skin lookups. Each client may send 1,000 requests per minute in total to every route except `/health`, reported in `RateLimit-*` headers."
    ),
    // The relative server targets whichever host serves this page. Production is listed for
    // pages that embed this document from another site.
    servers(
        (url = "/", description = "This server"),
        (url = "https://eclipse.skinsrestorer.net", description = "Production")
    ),
    paths(lookup_uuid, lookup_skin, health),
    // Listed so that the Mojang endpoints appear above the health check.
    tags((name = "mojang"), (name = "health"))
)]
struct ApiDoc;

/// Builds the application router.
///
/// `trusted_proxies` lists the peers whose `CF-Connecting-IP` header the rate limiter trusts.
pub fn build_router(state: AppState, trusted_proxies: Vec<IpNet>) -> Router {
    let rate_limiter = Arc::new(RateLimiter::new(
        RATE_LIMIT,
        RATE_LIMIT_WINDOW,
        trusted_proxies,
    ));

    let limited_routes = Router::new()
        .route("/mojang/uuid/{name}", get(lookup_uuid))
        .route("/mojang/skin/{uuid}", get(lookup_skin))
        .merge(docs::router(ApiDoc::openapi()))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(
            rate_limiter,
            enforce_rate_limit,
        ));
    // Health checks bypass the rate limit, so busy clients cannot fail platform probes.
    let unlimited_routes = Router::new()
        .route("/health", get(health))
        .method_not_allowed_fallback(method_not_allowed);

    // The last layer runs first: CORS wraps the rate limiter, so 429 responses carry CORS
    // headers and preflight requests do not use up a client's quota.
    unlimited_routes
        .merge(limited_routes)
        .with_state(state)
        .layer(middleware::from_fn_with_state(
            REQUEST_TIMEOUT,
            enforce_deadline,
        ))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET])
                .allow_headers(Any)
                // Lets browser scripts read `Retry-After` and the `RateLimit-*` headers.
                .expose_headers(Any),
        )
        .layer(middleware::map_response(set_nosniff))
        .layer(TraceLayer::new_for_http())
}

#[utoipa::path(
    get,
    path = "/mojang/uuid/{name}",
    params(("name" = String, Path, description = "Minecraft username to convert to UUID", example = "Pistonmaster")),
    responses(
        (status = 200, description = "`exists` is `false` when no player has this name", body = UuidLookupResponse,
            headers(("Cache-Control" = String, description = "`public, max-age=900` when `exists` is `true`, otherwise `public, max-age=60`"))),
        (status = 400, description = "The name is not 1 to 16 characters of `A-Z`, `a-z`, `0-9`, `_`, or `-`", body = ErrorResponse,
            example = json!({"error": "INVALID_NAME"})),
        (status = 429, description = "The client exceeded the rate limit", body = ErrorResponse,
            headers(("Retry-After" = u64, description = "Seconds to wait before retrying")),
            example = json!({"error": "RATE_LIMITED"})),
        (status = "5XX", description = "Mojang failed or is unavailable. The examples list each status and code.", body = ErrorResponse,
            headers(("Retry-After" = u64, description = "Seconds to wait before retrying, for `SERVICE_UNAVAILABLE`")),
            examples(
                ("500 INTERNAL_ERROR" = (summary = "500: the request to Mojang failed", value = json!({"error": "INTERNAL_ERROR"}))),
                ("502 UPSTREAM_ERROR" = (summary = "502: Mojang returned an unexpected status", value = json!({"error": "UPSTREAM_ERROR"}))),
                ("503 INTERNAL_TIMEOUT" = (summary = "503: Mojang timed out", value = json!({"error": "INTERNAL_TIMEOUT"}))),
                ("503 SERVICE_UNAVAILABLE" = (summary = "503: Mojang is rate-limiting this service, or this service is at capacity", value = json!({"error": "SERVICE_UNAVAILABLE"})))
            ))
    ),
    tag = "mojang"
)]
async fn lookup_uuid(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
) -> Result<Response, AppError> {
    state.metrics.increment_uuid_requests();
    let Path(name) = name
        .ok()
        .filter(|Path(name)| is_valid_minecraft_username(name))
        .ok_or(AppError::InvalidName)?;

    let uuid = state.batch_lookup.lookup(name).await?;
    Ok(cacheable_json(
        uuid.is_some(),
        UuidLookupResponse {
            exists: uuid.is_some(),
            uuid,
        },
    ))
}

#[utoipa::path(
    get,
    path = "/mojang/skin/{uuid}",
    params(("uuid" = String, Path, description = "Minecraft UUID to get skin data for, with or without dashes", example = "b1ae0778-4817-436c-96a3-a72c67cda060")),
    responses(
        (status = 200, description = "`exists` is `false` when the profile or its skin does not exist", body = SkinLookupResponse,
            headers(("Cache-Control" = String, description = "`public, max-age=900` when `exists` is `true`, otherwise `public, max-age=60`"))),
        (status = 400, description = "The UUID is not 32 hex digits, with or without dashes", body = ErrorResponse,
            example = json!({"error": "INVALID_UUID"})),
        (status = 429, description = "The client exceeded the rate limit", body = ErrorResponse,
            headers(("Retry-After" = u64, description = "Seconds to wait before retrying")),
            example = json!({"error": "RATE_LIMITED"})),
        (status = "5XX", description = "Mojang failed or is unavailable. The examples list each status and code.", body = ErrorResponse,
            headers(("Retry-After" = u64, description = "Seconds to wait before retrying, for `SERVICE_UNAVAILABLE`")),
            examples(
                ("500 INTERNAL_ERROR" = (summary = "500: the request to Mojang failed", value = json!({"error": "INTERNAL_ERROR"}))),
                ("502 UPSTREAM_ERROR" = (summary = "502: Mojang returned an unexpected status", value = json!({"error": "UPSTREAM_ERROR"}))),
                ("503 INTERNAL_TIMEOUT" = (summary = "503: Mojang timed out", value = json!({"error": "INTERNAL_TIMEOUT"}))),
                ("503 SERVICE_UNAVAILABLE" = (summary = "503: Mojang is rate-limiting this service, or this service is at capacity", value = json!({"error": "SERVICE_UNAVAILABLE"})))
            ))
    ),
    tag = "mojang"
)]
async fn lookup_skin(
    State(state): State<AppState>,
    uuid: Result<Path<String>, PathRejection>,
) -> Result<Response, AppError> {
    state.metrics.increment_skin_requests();
    let uuid = uuid
        .ok()
        .and_then(|Path(uuid)| parse_minecraft_uuid(&uuid))
        .ok_or(AppError::InvalidUuid)?;

    let mojang = Arc::clone(&state.mojang);
    let result = state
        .cache
        .get_or_try_insert_skin(uuid, async move {
            mojang
                .lookup_skin(uuid)
                .await
                .map(|property| property.map(Arc::new))
        })
        .await
        .map_err(|error| *error);
    // A failed load was still a miss, so count it before returning the error.
    if result.as_ref().is_ok_and(CacheLoad::was_hit) {
        state.metrics.increment_skin_cache_hits();
    } else {
        state.metrics.increment_skin_cache_misses();
    }
    let property = result?.into_value();

    Ok(cacheable_json(
        property.is_some(),
        BorrowedSkinLookupResponse {
            exists: property.is_some(),
            skin_property: property.as_deref(),
        },
    ))
}

#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Service is running", body = HealthResponse,
            example = json!({"status": "UP"})),
        (status = 503, description = "Username lookups have stopped", body = HealthResponse,
            example = json!({"status": "DOWN"}))
    ),
    tag = "health"
)]
async fn health(State(state): State<AppState>) -> (StatusCode, Json<HealthResponse>) {
    let (code, status) = if state.batch_lookup.is_available() {
        (StatusCode::OK, HealthStatus::Up)
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, HealthStatus::Down)
    };
    (code, Json(HealthResponse { status }))
}

async fn not_found() -> AppError {
    AppError::NotFound
}

async fn method_not_allowed() -> AppError {
    AppError::MethodNotAllowed
}

/// Answers with the documented `INTERNAL_TIMEOUT` error when a request takes too long.
async fn enforce_deadline(
    State(deadline): State<Duration>,
    request: Request,
    next: Next,
) -> Response {
    tokio::time::timeout(deadline, next.run(request))
        .await
        .unwrap_or_else(|_| AppError::Timeout.into_response())
}

/// Stops browsers from treating JSON responses as another content type.
async fn set_nosniff(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

fn cacheable_json(exists: bool, value: impl Serialize) -> Response {
    let mut response = Json(value).into_response();
    response.headers_mut().insert(
        CACHE_CONTROL,
        if exists {
            FOUND_CACHE_CONTROL
        } else {
            NOT_FOUND_CACHE_CONTROL
        },
    );
    response
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{
            Method, Request, StatusCode,
            header::{
                ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, CACHE_CONTROL,
                LOCATION, ORIGIN, RETRY_AFTER, X_CONTENT_TYPE_OPTIONS,
            },
        },
    };
    use serde_json::Value;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::{
        batch::{BatchConfig, BatchProcessor},
        cache::CacheManager,
        metrics::Metrics,
        mojang::{MojangService, UpstreamError},
        types::{ErrorResponse, ErrorType, SkinLookupResponse, SkinProperty, UuidLookupResponse},
    };

    use super::{AppState, build_router, enforce_deadline};

    struct FakeMojangService {
        uuid: Uuid,
        skin: Option<SkinProperty>,
    }

    #[async_trait]
    impl MojangService for FakeMojangService {
        async fn lookup_names(
            &self,
            names: &[String],
        ) -> Result<Vec<(String, Uuid)>, UpstreamError> {
            Ok(names
                .iter()
                .filter(|name| name.eq_ignore_ascii_case("Pistonmaster"))
                .map(|name| (name.clone(), self.uuid))
                .collect())
        }

        async fn lookup_skin(&self, uuid: Uuid) -> Result<Option<SkinProperty>, UpstreamError> {
            if uuid == self.uuid {
                Ok(self.skin.clone())
            } else if uuid.is_nil() {
                Err(UpstreamError::HttpStatus(500))
            } else {
                Ok(None)
            }
        }
    }

    fn test_app() -> (axum::Router, BatchProcessor, Uuid) {
        let (app, batch, uuid, _) = test_app_with_metrics();
        (app, batch, uuid)
    }

    fn test_app_with_metrics() -> (axum::Router, BatchProcessor, Uuid, Arc<Metrics>) {
        let uuid = Uuid::parse_str("b1ae0778-4817-436c-96a3-a72c67cda060")
            .expect("test UUID should parse");
        let mojang: Arc<dyn MojangService> = Arc::new(FakeMojangService {
            uuid,
            skin: Some(SkinProperty {
                value: "texture".to_owned(),
                signature: "signature".to_owned(),
            }),
        });
        let metrics = Arc::new(Metrics::default());
        let cache = CacheManager::default();
        let batch = BatchProcessor::start(
            Arc::clone(&mojang),
            cache.clone(),
            Arc::clone(&metrics),
            BatchConfig::new(10, Duration::from_millis(5), 32, 2)
                .expect("test batch configuration should be valid"),
        );
        let app = build_router(
            AppState {
                batch_lookup: batch.lookup(),
                cache,
                mojang,
                metrics: Arc::clone(&metrics),
            },
            Vec::new(),
        );
        (app, batch, uuid, metrics)
    }

    fn get(path: &str) -> Request<Body> {
        Request::get(path)
            .body(Body::empty())
            .expect("request should build")
    }

    async fn response_json<T: serde::de::DeserializeOwned>(
        response: axum::response::Response,
    ) -> T {
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body should be readable");
        serde_json::from_slice(&body).expect("response should contain valid JSON")
    }

    #[tokio::test]
    async fn serves_uuid_and_skin_contracts() {
        let (app, batch, uuid) = test_app();
        let uuid_response = app
            .clone()
            .oneshot(get("/mojang/uuid/Pistonmaster"))
            .await
            .expect("request should succeed");
        assert_eq!(uuid_response.status(), StatusCode::OK);
        assert_eq!(
            uuid_response.headers().get(CACHE_CONTROL),
            Some(&"public, max-age=900".parse().expect("header should parse"))
        );
        assert_eq!(
            response_json::<UuidLookupResponse>(uuid_response).await,
            UuidLookupResponse {
                exists: true,
                uuid: Some(uuid)
            }
        );

        let skin_response = app
            .oneshot(get(&format!("/mojang/skin/{uuid}")))
            .await
            .expect("request should succeed");
        assert_eq!(skin_response.status(), StatusCode::OK);
        assert_eq!(
            response_json::<SkinLookupResponse>(skin_response).await,
            SkinLookupResponse {
                exists: true,
                skin_property: Some(SkinProperty {
                    value: "texture".to_owned(),
                    signature: "signature".to_owned(),
                }),
            }
        );

        batch.shutdown().await;
    }

    #[tokio::test]
    async fn returns_typed_validation_errors() {
        let (app, batch, _) = test_app();
        let invalid_name = app
            .clone()
            .oneshot(get("/mojang/uuid/invalid%20name"))
            .await
            .expect("request should succeed");
        assert_eq!(invalid_name.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json::<ErrorResponse>(invalid_name).await,
            ErrorResponse {
                error: ErrorType::InvalidName
            }
        );

        let invalid_uuid = app
            .oneshot(get("/mojang/skin/not-a-uuid"))
            .await
            .expect("request should succeed");
        assert_eq!(invalid_uuid.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json::<ErrorResponse>(invalid_uuid).await,
            ErrorResponse {
                error: ErrorType::InvalidUuid
            }
        );

        batch.shutdown().await;
    }

    #[tokio::test]
    async fn serves_openapi_as_structured_json() {
        let (app, batch, _) = test_app();
        let response = app
            .oneshot(get("/openapi"))
            .await
            .expect("request should succeed");
        assert_eq!(response.status(), StatusCode::OK);
        let document: Value = response_json(response).await;
        let title = document.pointer("/info/title").and_then(Value::as_str);
        assert_eq!(title, Some("Mojang API Proxy"));

        let paths = document.get("paths").expect("OpenAPI paths should exist");
        for route in ["/mojang/uuid/{name}", "/mojang/skin/{uuid}", "/health"] {
            assert!(paths.get(route).is_some_and(Value::is_object));
        }
        let skin_responses = paths
            .pointer("/~1mojang~1skin~1{uuid}/get/responses")
            .and_then(Value::as_object)
            .expect("skin responses should exist");
        assert_eq!(
            skin_responses
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["200", "400", "429", "5XX"]
        );
        assert_eq!(
            document.pointer("/servers/0/url").and_then(Value::as_str),
            Some("/"),
            "the docs page should call the server that serves it"
        );

        batch.shutdown().await;
    }

    #[tokio::test]
    async fn returns_typed_errors_for_unknown_routes_and_methods() {
        let (app, batch, _) = test_app();
        let missing = app
            .clone()
            .oneshot(get("/does-not-exist"))
            .await
            .expect("request should succeed");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response_json::<ErrorResponse>(missing).await,
            ErrorResponse {
                error: ErrorType::NotFound
            }
        );

        for path in ["/health", "/mojang/uuid/Pistonmaster"] {
            let wrong_method = app
                .clone()
                .oneshot(
                    Request::post(path)
                        .body(Body::empty())
                        .expect("request should build"),
                )
                .await
                .expect("request should succeed");
            assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert_eq!(
                response_json::<ErrorResponse>(wrong_method).await,
                ErrorResponse {
                    error: ErrorType::MethodNotAllowed
                }
            );
        }

        let invalid_utf8 = app
            .oneshot(get("/mojang/uuid/%FF"))
            .await
            .expect("request should succeed");
        assert_eq!(invalid_utf8.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json::<ErrorResponse>(invalid_utf8).await,
            ErrorResponse {
                error: ErrorType::InvalidName
            }
        );

        batch.shutdown().await;
    }

    #[tokio::test]
    async fn caches_missing_profiles_briefly() {
        let (app, batch, _) = test_app();
        let response = app
            .oneshot(get("/mojang/uuid/UnknownPlayer"))
            .await
            .expect("request should succeed");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CACHE_CONTROL),
            Some(&"public, max-age=60".parse().expect("header should parse"))
        );

        batch.shutdown().await;
    }

    #[tokio::test]
    async fn counts_skin_cache_hits_and_misses_including_failures() {
        let (app, batch, uuid, metrics) = test_app_with_metrics();
        let skin = |uuid: Uuid| get(&format!("/mojang/skin/{uuid}"));

        for _ in 0..2 {
            let found = app
                .clone()
                .oneshot(skin(uuid))
                .await
                .expect("request should succeed");
            assert_eq!(found.status(), StatusCode::OK);
            assert_eq!(
                found.headers().get(CACHE_CONTROL),
                Some(&"public, max-age=900".parse().expect("header should parse"))
            );
        }

        let missing = app
            .clone()
            .oneshot(skin(Uuid::from_u128(1)))
            .await
            .expect("request should succeed");
        assert_eq!(missing.status(), StatusCode::OK);
        assert_eq!(
            missing.headers().get(CACHE_CONTROL),
            Some(&"public, max-age=60".parse().expect("header should parse"))
        );
        assert_eq!(
            response_json::<SkinLookupResponse>(missing).await,
            SkinLookupResponse {
                exists: false,
                skin_property: None,
            }
        );

        let failed = app
            .oneshot(skin(Uuid::nil()))
            .await
            .expect("request should succeed");
        assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
        assert!(!failed.headers().contains_key(CACHE_CONTROL));

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.skin_requests, 4);
        assert_eq!(
            (snapshot.skin_cache_hits, snapshot.skin_cache_misses),
            (1, 3),
            "a failed load was not served from the cache, so it is a miss"
        );

        batch.shutdown().await;
    }

    #[tokio::test]
    async fn rate_limits_with_cors_headers_and_exempts_health() {
        let (app, batch, _) = test_app();
        let request = |path: &str| {
            Request::get(path)
                .header(ORIGIN, "https://example.com")
                .body(Body::empty())
                .expect("request should build")
        };
        // The quota refills while the loop runs, so a few extra requests may pass.
        let mut limited = None;
        for _ in 0..super::RATE_LIMIT.get() * 2 {
            let response = app
                .clone()
                .oneshot(request("/does-not-exist"))
                .await
                .expect("request should succeed");
            if response.status() == StatusCode::TOO_MANY_REQUESTS {
                limited = Some(response);
                break;
            }
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
        let limited = limited.expect("the rate limit should reject a request");
        assert_eq!(
            limited.headers().get(ACCESS_CONTROL_ALLOW_ORIGIN),
            Some(&"*".parse().expect("header should parse")),
            "browsers need CORS headers to read a 429"
        );
        assert_eq!(
            limited.headers().get(ACCESS_CONTROL_EXPOSE_HEADERS),
            Some(&"*".parse().expect("header should parse")),
            "browser scripts need Retry-After and the RateLimit-* headers"
        );
        assert!(limited.headers().contains_key(RETRY_AFTER));
        assert_eq!(
            response_json::<ErrorResponse>(limited).await,
            ErrorResponse {
                error: ErrorType::RateLimited
            }
        );

        let preflight = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/mojang/uuid/Pistonmaster")
                    .header(ORIGIN, "https://example.com")
                    .header("access-control-request-method", "GET")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should succeed");
        assert_eq!(preflight.status(), StatusCode::OK);

        let health = app
            .oneshot(request("/health"))
            .await
            .expect("request should succeed");
        assert_eq!(health.status(), StatusCode::OK);

        batch.shutdown().await;
    }

    #[tokio::test]
    async fn answers_slow_requests_with_a_typed_timeout() {
        let app = axum::Router::new()
            .route(
                "/slow",
                axum::routing::get(|| tokio::time::sleep(Duration::from_secs(60))),
            )
            .layer(axum::middleware::from_fn_with_state(
                Duration::from_millis(10),
                enforce_deadline,
            ));
        let response = app
            .oneshot(get("/slow"))
            .await
            .expect("request should succeed");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response_json::<ErrorResponse>(response).await,
            ErrorResponse {
                error: ErrorType::InternalTimeout
            }
        );
    }

    #[tokio::test]
    async fn reports_unhealthy_after_batching_stops() {
        let (app, batch, _) = test_app();
        batch.shutdown().await;

        let response = app
            .oneshot(get("/health"))
            .await
            .expect("request should succeed");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: Value = response_json(response).await;
        assert_eq!(body.get("status").and_then(Value::as_str), Some("DOWN"));
    }

    #[tokio::test]
    async fn serves_mobile_friendly_docs_and_security_headers() {
        let (app, batch, _) = test_app();

        let health = app
            .clone()
            .oneshot(get("/health"))
            .await
            .expect("request should succeed");
        assert_eq!(
            health.headers().get(X_CONTENT_TYPE_OPTIONS),
            Some(&"nosniff".parse().expect("header should parse")),
            "routes outside the rate limiter also get nosniff"
        );

        let page = app
            .clone()
            .oneshot(get("/swagger/"))
            .await
            .expect("request should succeed");
        assert_eq!(page.status(), StatusCode::OK);
        let html = to_bytes(page.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let html = String::from_utf8(html.to_vec()).expect("page should be UTF-8");
        assert!(
            html.contains(
                r#"<meta name="viewport" content="width=device-width, initial-scale=1">"#
            ),
            "phones need a viewport tag to use their own width"
        );

        let initializer = app
            .clone()
            .oneshot(get("/swagger/swagger-initializer.js"))
            .await
            .expect("request should succeed");
        assert_eq!(initializer.status(), StatusCode::OK);
        let script = to_bytes(initializer.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        let script = String::from_utf8(script.to_vec()).expect("script should be UTF-8");
        assert!(script.contains(r#""url": "/openapi""#), "{script}");
        assert!(script.contains(r#""layout": "BaseLayout""#), "{script}");

        let favicon = app
            .clone()
            .oneshot(get("/favicon.ico"))
            .await
            .expect("request should succeed");
        assert_eq!(favicon.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            favicon.headers().get(LOCATION),
            Some(
                &"/swagger/favicon-32x32.png"
                    .parse()
                    .expect("header should parse")
            )
        );

        let missing = app
            .oneshot(get("/swagger/missing.js"))
            .await
            .expect("request should succeed");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        batch.shutdown().await;
    }
}
