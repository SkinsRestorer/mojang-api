//! Serves the `OpenAPI` document and Swagger UI.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::Path,
    http::{HeaderValue, header::CONTENT_TYPE},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use tracing::error;
use utoipa::openapi::OpenApi;
use utoipa_swagger_ui::Config;

use crate::error::AppError;

const OPENAPI_PATH: &str = "/openapi";
/// Replaces Swagger UI's stock page, which lacks a viewport tag and so renders at desktop
/// width on phones.
const INDEX_HTML: &str = include_str!("swagger-ui.html");

/// Routes for `/openapi`, `/swagger`, and the site favicon.
pub fn router<S: Clone + Send + Sync + 'static>(openapi: OpenApi) -> Router<S> {
    let config = Arc::new(
        Config::new([OPENAPI_PATH])
            // Drop the "Explore" bar, which only lets readers load a different spec.
            .use_base_layout()
            // Every response already links its schema, so hide the duplicate list.
            .default_models_expand_depth(-1),
    );
    Router::new()
        .route(
            OPENAPI_PATH,
            get(move || {
                let openapi = openapi.clone();
                async move { Json(openapi) }
            }),
        )
        .route("/", get(|| async { Redirect::temporary("/swagger/") }))
        .route(
            "/swagger",
            get(|| async { Redirect::permanent("/swagger/") }),
        )
        .route("/swagger/", get(|| async { Html(INDEX_HTML) }))
        .route("/swagger/index.html", get(|| async { Html(INDEX_HTML) }))
        .route(
            "/swagger/{*file}",
            get(move |Path(file): Path<String>| {
                let config = Arc::clone(&config);
                async move { swagger_asset(&file, config) }
            }),
        )
        .route(
            "/favicon.ico",
            get(|| async { Redirect::permanent("/swagger/favicon-32x32.png") }),
        )
}

fn swagger_asset(file: &str, config: Arc<Config<'static>>) -> Response {
    match utoipa_swagger_ui::serve(file, config) {
        Ok(Some(asset)) => {
            // Serves embedded files without copying them. Copying the 1.4 MB bundle for each
            // request would let clients that read slowly hold gigabytes of memory.
            let mut response = asset.bytes.into_response();
            if let Ok(content_type) = HeaderValue::from_str(&asset.content_type) {
                response.headers_mut().insert(CONTENT_TYPE, content_type);
            }
            response
        }
        Ok(None) => AppError::NotFound.into_response(),
        Err(error) => {
            error!(%error, file, "could not serve Swagger UI asset");
            AppError::Internal.into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;
    use utoipa::openapi::OpenApi;

    use super::router;

    #[tokio::test]
    async fn serves_embedded_assets_without_copying_them() {
        let app = router::<()>(OpenApi::default());
        let mut bodies = Vec::new();
        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::get("/swagger/swagger-ui-bundle.js")
                        .body(Body::empty())
                        .expect("request should build"),
                )
                .await
                .expect("request should succeed");
            assert_eq!(response.status(), StatusCode::OK);
            bodies.push(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("body should be readable"),
            );
        }
        assert!(bodies[0].len() > 1024 * 1024, "{}", bodies[0].len());
        assert_eq!(
            bodies[0].as_ptr(),
            bodies[1].as_ptr(),
            "each response should share the embedded bytes instead of a new copy"
        );
    }
}
