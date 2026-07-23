use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};

use crate::{
    mojang::UpstreamError,
    types::{ErrorResponse, ErrorType},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppError {
    InvalidName,
    InvalidUuid,
    Timeout,
    Internal,
}

impl From<UpstreamError> for AppError {
    fn from(error: UpstreamError) -> Self {
        match error {
            UpstreamError::Timeout => Self::Timeout,
            UpstreamError::HttpStatus(_)
            | UpstreamError::Transport
            | UpstreamError::InvalidResponse => Self::Internal,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            Self::InvalidName => (StatusCode::BAD_REQUEST, ErrorType::InvalidName),
            Self::InvalidUuid => (StatusCode::BAD_REQUEST, ErrorType::InvalidUuid),
            Self::Timeout => (StatusCode::SERVICE_UNAVAILABLE, ErrorType::InternalTimeout),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, ErrorType::InternalError),
        };
        (status, Json(ErrorResponse { error })).into_response()
    }
}
