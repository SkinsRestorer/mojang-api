use std::{fmt, time::Duration};

use axum::{
    Json,
    http::{HeaderValue, StatusCode, header::RETRY_AFTER},
    response::{IntoResponse, Response},
};

use crate::{
    mojang::UpstreamError,
    types::{ErrorResponse, ErrorType},
};

#[derive(Debug, Clone, Copy)]
pub enum AppError {
    InvalidName,
    InvalidUuid,
    NotFound,
    MethodNotAllowed,
    Timeout,
    /// Mojang rate-limited us or local capacity is exhausted.
    Unavailable {
        retry_after: Duration,
    },
    /// Mojang answered with an unexpected HTTP status.
    BadGateway,
    Internal,
}

impl From<UpstreamError> for AppError {
    fn from(error: UpstreamError) -> Self {
        match error {
            UpstreamError::Timeout => Self::Timeout,
            UpstreamError::RateLimited { retry_after } => Self::Unavailable { retry_after },
            UpstreamError::Overloaded => Self::Unavailable {
                retry_after: Duration::from_secs(1),
            },
            UpstreamError::HttpStatus(_) => Self::BadGateway,
            UpstreamError::Transport | UpstreamError::InvalidResponse => Self::Internal,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::InvalidName => (StatusCode::BAD_REQUEST, ErrorType::InvalidName),
            Self::InvalidUuid => (StatusCode::BAD_REQUEST, ErrorType::InvalidUuid),
            Self::NotFound => (StatusCode::NOT_FOUND, ErrorType::NotFound),
            Self::MethodNotAllowed => (StatusCode::METHOD_NOT_ALLOWED, ErrorType::MethodNotAllowed),
            Self::Timeout => (StatusCode::SERVICE_UNAVAILABLE, ErrorType::InternalTimeout),
            Self::Unavailable { .. } => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorType::ServiceUnavailable,
            ),
            Self::BadGateway => (StatusCode::BAD_GATEWAY, ErrorType::UpstreamError),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, ErrorType::InternalError),
        };
        let mut response = (status, Json(ErrorResponse { error })).into_response();
        if let Self::Unavailable { retry_after } = self {
            response
                .headers_mut()
                .insert(RETRY_AFTER, HeaderValue::from(ceil_seconds(retry_after)));
        }
        response
    }
}

/// Rounds a duration up to whole seconds, with a minimum of one second, for `Retry-After`.
#[must_use]
pub fn ceil_seconds(duration: Duration) -> u64 {
    duration
        .as_secs()
        .saturating_add(u64::from(duration.subsec_nanos() > 0))
        .max(1)
}

/// Displays an error followed by each of its sources, separated by `: `.
pub struct ErrorChain<'a>(pub &'a (dyn std::error::Error + 'static));

impl fmt::Display for ErrorChain<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(error) = source {
            write!(formatter, ": {error}")?;
            source = error.source();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::{
        body::to_bytes,
        http::{StatusCode, header::RETRY_AFTER},
        response::IntoResponse,
    };

    use crate::mojang::UpstreamError;

    use super::{AppError, ceil_seconds};

    async fn render(error: UpstreamError) -> (StatusCode, Option<String>, String) {
        let response = AppError::from(error).into_response();
        let status = response.status();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .map(|value| value.to_str().expect("header should be ASCII").to_owned());
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should be readable");
        (
            status,
            retry_after,
            String::from_utf8(body.to_vec()).expect("body should be UTF-8"),
        )
    }

    #[tokio::test]
    async fn maps_upstream_failures_to_documented_responses() {
        assert_eq!(
            render(UpstreamError::Timeout).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                None,
                r#"{"error":"INTERNAL_TIMEOUT"}"#.to_owned()
            )
        );
        assert_eq!(
            render(UpstreamError::RateLimited {
                retry_after: Duration::from_millis(1_500)
            })
            .await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Some("2".to_owned()),
                r#"{"error":"SERVICE_UNAVAILABLE"}"#.to_owned()
            )
        );
        assert_eq!(
            render(UpstreamError::Overloaded).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Some("1".to_owned()),
                r#"{"error":"SERVICE_UNAVAILABLE"}"#.to_owned()
            )
        );
        assert_eq!(
            render(UpstreamError::Transport).await,
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
                r#"{"error":"INTERNAL_ERROR"}"#.to_owned()
            )
        );
        assert_eq!(
            render(UpstreamError::HttpStatus(500)).await,
            (
                StatusCode::BAD_GATEWAY,
                None,
                r#"{"error":"UPSTREAM_ERROR"}"#.to_owned()
            )
        );
        assert_eq!(
            render(UpstreamError::InvalidResponse).await,
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
                r#"{"error":"INTERNAL_ERROR"}"#.to_owned()
            )
        );
    }

    #[test]
    fn rounds_retry_delays_up() {
        assert_eq!(ceil_seconds(Duration::ZERO), 1);
        assert_eq!(ceil_seconds(Duration::from_millis(1_001)), 2);
        assert_eq!(ceil_seconds(Duration::from_secs(3)), 3);
    }
}
