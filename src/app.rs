use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderValue, Method, header::CACHE_CONTROL},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use serde::Serialize;
use tower_http::{
    cors::{Any, CorsLayer},
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
    types::{
        ErrorResponse, HealthResponse, NotFoundResponse, SkinLookupResponse, SkinProperty,
        UuidLookupResponse,
    },
    validation::{is_valid_minecraft_username, parse_minecraft_uuid},
};

const CLIENT_CACHE_CONTROL: HeaderValue = HeaderValue::from_static("public, max-age=900");

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
        description = "A proxy service for Mojang API endpoints"
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
/// # Errors
///
/// Returns an error when the built-in rate limiter configuration is invalid.
pub fn build_router(state: AppState, local_port: u16) -> Result<Router, RateLimitConfigError> {
    let mut openapi = ApiDoc::openapi();
    openapi
        .servers
        .get_or_insert_default()
        .push(Server::new(format!("http://localhost:{local_port}")));
    let rate_limiter = Arc::new(RateLimiter::new(1_000, Duration::from_mins(1))?);

    Ok(Router::new()
        .route("/", get(|| async { Redirect::temporary("/swagger") }))
        .route("/health", get(health))
        .route("/mojang/uuid/{name}", get(lookup_uuid))
        .route("/mojang/skin/{uuid}", get(lookup_skin))
        .merge(SwaggerUi::new("/swagger").url("/openapi", openapi))
        .fallback(not_found)
        .with_state(state)
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET])
                .allow_headers(Any),
        )
        .layer(middleware::from_fn_with_state(
            rate_limiter,
            enforce_rate_limit,
        ))
        .layer(TraceLayer::new_for_http()))
}

#[utoipa::path(
    get,
    path = "/mojang/uuid/{name}",
    params(("name" = String, Path, description = "Minecraft username to convert to UUID")),
    responses(
        (status = 200, description = "Successful response", body = UuidLookupResponse),
        (status = 400, description = "Invalid username format", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse),
        (status = 503, description = "Service unavailable due to timeout", body = ErrorResponse)
    ),
    tag = "mojang"
)]
async fn lookup_uuid(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Response, AppError> {
    state.metrics.increment_uuid_requests();
    if !is_valid_minecraft_username(&name) {
        return Err(AppError::InvalidName);
    }

    let uuid = state.batch_lookup.lookup(name).await?;
    Ok(cacheable_json(UuidLookupResponse {
        exists: uuid.is_some(),
        uuid,
    }))
}

#[utoipa::path(
    get,
    path = "/mojang/skin/{uuid}",
    params(("uuid" = String, Path, description = "Minecraft UUID to get skin data for")),
    responses(
        (status = 200, description = "Successful response", body = SkinLookupResponse),
        (status = 400, description = "Invalid UUID format", body = ErrorResponse),
        (status = 500, description = "Internal server error", body = ErrorResponse),
        (status = 503, description = "Service unavailable due to timeout", body = ErrorResponse)
    ),
    tag = "mojang"
)]
async fn lookup_skin(
    State(state): State<AppState>,
    Path(uuid): Path<String>,
) -> Result<Response, AppError> {
    state.metrics.increment_skin_requests();
    let uuid = parse_minecraft_uuid(&uuid).ok_or(AppError::InvalidUuid)?;

    let mojang = Arc::clone(&state.mojang);
    let metrics = Arc::clone(&state.metrics);
    let result = state
        .cache
        .get_or_try_insert_skin(uuid, async move {
            metrics.increment_skin_cache_misses();
            mojang
                .lookup_skin(uuid)
                .await
                .map(|property| property.map(Arc::new))
        })
        .await
        .map_err(|error| *error)?;
    if !result.was_loaded() {
        state.metrics.increment_skin_cache_hits();
    }
    let property = result.into_value();

    Ok(cacheable_json(BorrowedSkinLookupResponse {
        exists: property.is_some(),
        skin_property: property.as_deref(),
    }))
}

#[utoipa::path(
    get,
    path = "/health",
    responses(
        (status = 200, description = "Service is running", body = HealthResponse)
    ),
    tag = "health"
)]
async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "UP" })
}

async fn not_found() -> impl IntoResponse {
    (
        axum::http::StatusCode::NOT_FOUND,
        Json(NotFoundResponse { error: "Not Found" }),
    )
}

fn cacheable_json(value: impl serde::Serialize) -> Response {
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, CLIENT_CACHE_CONTROL);
    response
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header::CACHE_CONTROL},
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
        )
        .expect("test router configuration should be valid");
        (app, batch, uuid)
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
        assert_eq!(document["info"]["title"], "Mojang API Proxy");
        assert!(document["paths"]["/mojang/uuid/{name}"].is_object());
        assert!(document["paths"]["/mojang/skin/{uuid}"].is_object());
        assert!(document["paths"]["/health"].is_object());

        batch.shutdown().await;
    }
}
