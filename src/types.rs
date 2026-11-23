use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct UuidLookupResponse {
    pub exists: bool,
    #[schema(example = "b1ae0778-4817-436c-96a3-a72c67cda060")]
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
    /// Base64-encoded JSON with the texture URLs.
    #[schema(
        example = "eyJ0aW1lc3RhbXAiOjE3MDAwMDAwMDAwMDAsInByb2ZpbGVJZCI6ImIxYWUwNzc4NDgxNzQzNmM5NmEzYTcyYzY3Y2RhMDYwIiwicHJvZmlsZU5hbWUiOiJQaXN0b25tYXN0ZXIiLCJ0ZXh0dXJlcyI6eyJTS0lOIjp7InVybCI6Imh0dHA6Ly90ZXh0dXJlcy5taW5lY3JhZnQubmV0L3RleHR1cmUvOGIzZjJjMWY2YzFmNGUxYjlhMmQ3YzVlNGYzYTJiMWMwZDllOGY3YTZiNWM0ZDNlMmYxYTBiOWM4ZDdlNmY1YSJ9fX0="
    )]
    pub value: String,
    /// Mojang's Base64-encoded signature of `value`. Real signatures are 684 characters.
    #[schema(example = "U2lnbmVkIGJ5IE1vamFuZw==")]
    pub signature: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorType {
    InvalidName,
    InvalidUuid,
    InternalTimeout,
    InternalError,
    /// Mojang rate-limited this service or the service is at capacity. Retry after `Retry-After`.
    ServiceUnavailable,
    /// Mojang returned an unexpected HTTP status.
    UpstreamError,
    /// The client exceeded this service's rate limit. Retry after `Retry-After`.
    RateLimited,
    NotFound,
    MethodNotAllowed,
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
    /// Mojang signs `textures`; other properties may be unsigned.
    pub signature: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct HealthResponse {
    pub status: HealthStatus,
}

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum HealthStatus {
    Up,
    Down,
}
