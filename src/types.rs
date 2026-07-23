use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct UuidLookupResponse {
    pub exists: bool,
    pub uuid: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SkinLookupResponse {
    pub exists: bool,
    pub skin_property: Option<SkinProperty>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct SkinProperty {
    pub value: String,
    pub signature: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorType {
    InvalidName,
    InvalidUuid,
    InternalTimeout,
    InternalError,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct ErrorResponse {
    pub error: ErrorType,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MojangBatchProfile {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MojangProfile {
    #[serde(default)]
    pub properties: Vec<MojangProperty>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MojangProperty {
    pub name: String,
    pub value: String,
    pub signature: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct HealthResponse {
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct NotFoundResponse {
    pub error: &'static str,
}

#[derive(Debug, Serialize)]
pub(crate) struct RateLimitResponse {
    pub error: &'static str,
}
