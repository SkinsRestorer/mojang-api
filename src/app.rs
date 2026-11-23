#![warn(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, State, rejection::PathRejection},
    http::{HeaderValue, Method, StatusCode, header::CACHE_CONTROL},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use ipnet::IpNet;
use serde::Serialize;
use tower_http::{
    cors::{Any, CorsLayer},
    timeout::TimeoutLayer,
    trace::TraceLayer,
};
use utoipa::{
    Modify, OpenApi,
    openapi::{OpenApi as OpenApiDocument, Server},
};
use utoipa_swagger_ui::SwaggerUi;

use crate::{
    batch::BatchLookup,
    cache::CacheManager,
    error::AppError,
    metrics::Metrics,
    mojang::MojangService,
    rate_limit::{RateLimitConfigError, RateLimiter, enforce_rate_limit},
    types::{ErrorResponse, HealthResponse, SkinLookupResponse, SkinProperty, UuidLookupResponse},
    validation::{is_valid_minecraft_username, parse_minecraft_uuid},
};

const RATE_LIMIT: u32 = 1_000;
const RATE_LIMIT_WINDOW: Duration = Duration::from_mins(1);
/// A last-resort deadline. Lookups normally fail sooner with a typed JSON error.
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
        version = "2.0.0",
        description = "A proxy service for Mojang API endpoints.\n\nEach client may send 1,000 requests per minute to the `/mojang` endpoints. Responses include `RateLimit-Limit`, `RateLimit-Policy`, `RateLimit-Remaining`, and `RateLimit-Reset` headers. A client over the limit receives `429` with a `Retry-After` header."
    ),
    paths(lookup_uuid, lookup_skin, health),
    components(schemas(
        ErrorResponse,
        HealthResponse,
        SkinLookupResponse,
        SkinProperty,
        UuidLookupResponse
    )),
    tags(
        (name = "mojang", description = "Mojang API endpoints"),
        (name = "health", description = "Health check endpoint")
    ),
    modifiers(&ApiServers)
)]
struct ApiDoc;

struct ApiServers;

impl Modify for ApiServers {
    fn modify(&self, openapi: &mut OpenApiDocument) {
        openapi.servers = Some(vec![Server::new("https://eclipse.skinsrestorer.net")]);
    }
}

/// Builds the application router.
///
/// `trusted_proxies` lists the peers whose `CF-Connecting-IP` header the rate limiter trusts.
///
/// # Errors
///
/// Returns an error when the built-in rate limiter configuration is invalid.
pub fn build_router(
    state: AppState,
    local_port: u16,
    trusted_proxies: Vec<IpNet>,
) -> Result<Router, RateLimitConfigError> {
    let mut openapi = ApiDoc::openapi();
    openapi
        .servers
        .get_or_insert_default()
        .push(Server::new(format!("http://localhost:{local_port}")));
    let rate_limiter = Arc::new(RateLimiter::new(
        RATE_LIMIT,
        RATE_LIMIT_WINDOW,
        trusted_proxies,
    )?);

    let limited_routes = Router::new()
        .route("/", get(|| async { Redirect::temporary("/swagger") }))
        .route("/mojang/uuid/{name}", get(lookup_uuid))
        .route("/mojang/skin/{uuid}", get(lookup_skin))
        .merge(SwaggerUi::new("/swagger").url("/openapi", openapi))
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
    Ok(unlimited_routes
        .merge(limited_routes)
        .with_state(state)
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            REQUEST_TIMEOUT,
        ))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET])
                .allow_headers(Any),
        )
        .layer(TraceLayer::new_for_http()))
}

#[utoipa::path(
    get,
    path = "/mojang/uuid/{name}",
    params(("name" = String, Path, description = "Minecraft username to convert to UUID")),
    responses(
        (status = 200, description = "Successful response", body = UuidLookupResponse),
        (status = 400, description = "Invalid username format", body = ErrorResponse),
        (status = 429, description = "The client exceeded the rate limit", body = ErrorResponse,
            headers(("Retry-After" = u64, description = "Seconds to wait before retrying"))),
        (status = 500, description = "Internal server error", body = ErrorResponse),
        (status = 502, description = "Mojang returned an unexpected status", body = ErrorResponse),
        (status = 503, description = "Mojang timed out (`INTERNAL_TIMEOUT`), or Mojang rate-limited the service or the service is at capacity (`SERVICE_UNAVAILABLE`, with `Retry-After`)", body = ErrorResponse)
    ),
    tag = "mojang"
)]
async fn lookup_uuid(
    State(state): State<AppState>,
    name: Result<Path<String>, PathRejection>,
) -> Result<Response, AppError> {
    state.metrics.increment_uuid_requests();
    let Ok(Path(name)) = name else {
        return Err(AppError::InvalidName);
    };
    if !is_valid_minecraft_username(&name) {
        return Err(AppError::InvalidName);
    }

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
    params(("uuid" = String, Path, description = "Minecraft UUID to get skin data for")),
    responses(
        (status = 200, description = "Successful response", body = SkinLookupResponse),
        (status = 400, description = "Invalid UUID format", body = ErrorResponse),
        (status = 429, description = "The client exceeded the rate limit", body = ErrorResponse,
            headers(("Retry-After" = u64, description = "Seconds to wait before retrying"))),
        (status = 500, description = "Internal server error", body = ErrorResponse),
        (status = 502, description = "Mojang returned an unexpected status", body = ErrorResponse),
        (status = 503, description = "Mojang timed out (`INTERNAL_TIMEOUT`), or Mojang rate-limited the service or the service is at capacity (`SERVICE_UNAVAILABLE`, with `Retry-After`)", body = ErrorResponse)
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
        .map_err(|error| *error)?;
    if result.was_hit() {
        state.metrics.increment_skin_cache_hits();
    } else {
        state.metrics.increment_skin_cache_misses();
    }
    let property = result.into_value();

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
        (status = 200, description = "Service is running", body = HealthResponse),
        (status = 503, description = "Username lookups are unavailable", body = HealthResponse)
    ),
    tag = "health"
)]
async fn health(State(state): State<AppState>) -> (StatusCode, Json<HealthResponse>) {
    if state.batch_lookup.is_available() {
        (StatusCode::OK, Json(HealthResponse { status: "UP" }))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(HealthResponse { status: "DOWN" }),
        )
    }
}

async fn not_found() -> AppError {
    AppError::NotFound
}

async fn method_not_allowed() -> AppError {
    AppError::MethodNotAllowed
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
            header::{ACCESS_CONTROL_ALLOW_ORIGIN, CACHE_CONTROL, ORIGIN, RETRY_AFTER},
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

    use super::{AppState, build_router};

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

        async fn lookup_skin(&self, _uuid: Uuid) -> Result<Option<SkinProperty>, UpstreamError> {
            Ok(self.skin.clone())
        }
    }

    fn test_app() -> (axum::Router, BatchProcessor, Uuid) {
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
        let cache = CacheManager::new(32, Duration::from_mins(1))
            .expect("test cache configuration should be valid");
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
                metrics,
            },
            3000,
            Vec::new(),
        )
        .expect("test router configuration should be valid");
        (app, batch, uuid)
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
            .oneshot(
                Request::get("/mojang/uuid/Pistonmaster")
                    .body(Body::empty())
                    .expect("request should build"),
            )
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
            .oneshot(
                Request::get(format!("/mojang/skin/{uuid}"))
                    .body(Body::empty())
                    .expect("request should build"),
            )
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
            .oneshot(
                Request::get("/mojang/uuid/invalid%20name")
                    .body(Body::empty())
                    .expect("request should build"),
            )
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
            .oneshot(
                Request::get("/mojang/skin/not-a-uuid")
                    .body(Body::empty())
                    .expect("request should build"),
            )
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
            .oneshot(
                Request::get("/openapi")
                    .body(Body::empty())
                    .expect("request should build"),
            )
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
        for _ in 0..super::RATE_LIMIT * 2 {
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
}
