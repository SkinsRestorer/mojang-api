#![warn(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use reqwest::{
    Client, Proxy, RequestBuilder, StatusCode, Url,
    header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, HeaderMap, HeaderValue, RETRY_AFTER},
};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::{error, warn};
use uuid::Uuid;

use crate::{
    error::ErrorChain,
    http_client::http_client_builder,
    metrics::Metrics,
    types::{MojangBatchProfile, MojangProfile, SkinProperty},
    validation::parse_minecraft_uuid,
};

const MAX_UPSTREAM_RESPONSE_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Limits concurrent session server requests, so random UUIDs cannot flood Mojang.
const MAX_CONCURRENT_SKIN_LOOKUPS: usize = 64;
const DEFAULT_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_RATE_LIMIT_COOLDOWN: Duration = Duration::from_mins(10);
/// A rate-limited request is retried once through another outbound route.
const MAX_ATTEMPTS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UpstreamError {
    #[error("the Mojang request timed out")]
    Timeout,
    #[error("Mojang rate-limited every outbound route")]
    RateLimited { retry_after: Duration },
    #[error("too many Mojang requests are already in progress")]
    Overloaded,
    #[error("Mojang returned HTTP {0}")]
    HttpStatus(u16),
    #[error("the Mojang request failed")]
    Transport,
    #[error("Mojang returned an invalid response")]
    InvalidResponse,
}

#[async_trait]
pub trait MojangService: Send + Sync {
    async fn lookup_names(&self, names: &[String]) -> Result<Vec<(String, Uuid)>, UpstreamError>;

    async fn lookup_skin(&self, uuid: Uuid) -> Result<Option<SkinProperty>, UpstreamError>;
}

/// Mojang rate-limits each API separately, so cooldowns are tracked per API.
#[derive(Debug, Clone, Copy)]
enum Api {
    Batch,
    Profile,
}

/// One outbound path to Mojang: a direct connection or a single proxy.
///
/// Each route has its own client because pooled connections stay bound to the proxy that
/// opened them.
#[derive(Debug)]
struct Route {
    client: Client,
    /// For each API, milliseconds after `MojangHttpClient::epoch` until the route may be used.
    cooldown_until: [AtomicU64; 2],
}

impl Route {
    const fn cooldown(&self, api: Api) -> &AtomicU64 {
        match api {
            Api::Batch => &self.cooldown_until[0],
            Api::Profile => &self.cooldown_until[1],
        }
    }
}

#[derive(Debug)]
struct MojangEndpoints {
    batch_urls: Vec<Url>,
    profile_base_url: Url,
}

impl MojangEndpoints {
    #[expect(
        clippy::expect_used,
        reason = "The production endpoints are constant, valid URLs, which a test checks."
    )]
    fn production() -> Self {
        let parse = |url| Url::parse(url).expect("production Mojang URLs are valid");
        Self {
            batch_urls: vec![
                parse("https://api.mojang.com/profiles/minecraft"),
                parse("https://api.minecraftservices.com/minecraft/profile/lookup/bulk/byname"),
            ],
            profile_base_url: parse("https://sessionserver.mojang.com/session/minecraft/profile/"),
        }
    }
}

#[derive(Debug)]
pub struct MojangHttpClient {
    routes: Vec<Route>,
    epoch: Instant,
    endpoints: MojangEndpoints,
    metrics: Arc<Metrics>,
    skin_lookups: Semaphore,
}

impl MojangHttpClient {
    /// Creates the Mojang HTTP clients and loads any configured proxies.
    ///
    /// # Errors
    ///
    /// Returns an error when the proxy list cannot be read or an HTTP client cannot be built.
    pub fn new(
        proxy_list_file: Option<&Path>,
        metrics: Arc<Metrics>,
    ) -> Result<Self, ClientBuildError> {
        let proxies = proxy_list_file
            .map(load_proxy_list)
            .transpose()?
            .unwrap_or_default();
        Self::build(
            MojangEndpoints::production(),
            &proxies,
            REQUEST_TIMEOUT,
            metrics,
            true,
        )
    }

    fn build(
        endpoints: MojangEndpoints,
        proxies: &[Url],
        timeout: Duration,
        metrics: Arc<Metrics>,
        https_only: bool,
    ) -> Result<Self, ClientBuildError> {
        let mut default_headers = HeaderMap::new();
        default_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        default_headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en"));
        let builder = || {
            http_client_builder(timeout)
                .default_headers(default_headers.clone())
                .https_only(https_only)
        };
        let route = |client| Route {
            client,
            cooldown_until: [AtomicU64::new(0), AtomicU64::new(0)],
        };

        let routes = if proxies.is_empty() {
            vec![route(builder().build()?)]
        } else {
            let routes = proxies
                .iter()
                .map(|proxy| Ok(route(builder().proxy(Proxy::all(proxy.clone())?).build()?)))
                .collect::<Result<Vec<_>, ClientBuildError>>()?;
            tracing::info!(proxy_count = routes.len(), "loaded outbound proxies");
            routes
        };

        Ok(Self {
            routes,
            epoch: Instant::now(),
            endpoints,
            metrics,
            skin_lookups: Semaphore::new(MAX_CONCURRENT_SKIN_LOOKUPS),
        })
    }

    fn now_millis(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Picks a random route that Mojang has not rate-limited for `api`.
    fn choose_route(&self, api: Api) -> Result<&Route, UpstreamError> {
        let now = self.now_millis();
        let start = random_index(self.routes.len());
        self.routes
            .iter()
            .cycle()
            .skip(start)
            .take(self.routes.len())
            .find(|route| route.cooldown(api).load(Ordering::Relaxed) <= now)
            .ok_or_else(|| self.rate_limited(api))
    }

    /// Reports how long until the first route becomes available for `api` again.
    fn rate_limited(&self, api: Api) -> UpstreamError {
        let now = self.now_millis();
        let available_at = self
            .routes
            .iter()
            .map(|route| route.cooldown(api).load(Ordering::Relaxed))
            .min()
            .unwrap_or(now);
        UpstreamError::RateLimited {
            retry_after: Duration::from_millis(available_at.saturating_sub(now)),
        }
    }

    async fn request(
        &self,
        api: Api,
        sent_bytes: usize,
        build: impl Fn(&Client) -> RequestBuilder,
    ) -> Result<(StatusCode, Bytes), UpstreamError> {
        for attempt in 0..MAX_ATTEMPTS {
            let route = self.choose_route(api).inspect_err(|_| {
                // A retry follows a 429 that already counted as an error.
                if attempt == 0 {
                    self.metrics.increment_mojang_cooldown_rejections();
                }
            })?;
            match self
                .send(route, api, build(&route.client), sent_bytes)
                .await
            {
                Err(UpstreamError::RateLimited { .. }) => {}
                result => return result,
            }
        }
        // The last route's cooldown can outlast one that an earlier attempt rate-limited.
        Err(self.rate_limited(api))
    }

    /// Sends a request and reads the body of successful responses.
    ///
    /// Unsuccessful responses return their status with an empty body.
    async fn send(
        &self,
        route: &Route,
        api: Api,
        request: RequestBuilder,
        sent_bytes: usize,
    ) -> Result<(StatusCode, Bytes), UpstreamError> {
        self.metrics.record_mojang_request(sent_bytes);

        let mut response = request
            .send()
            .await
            .map_err(|error| self.record_transport_error(&error, "Mojang request failed"))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse().ok())
                .map_or(DEFAULT_RATE_LIMIT_COOLDOWN, Duration::from_secs)
                .clamp(Duration::from_secs(1), MAX_RATE_LIMIT_COOLDOWN);
            let available_at = self
                .now_millis()
                .saturating_add(u64::try_from(retry_after.as_millis()).unwrap_or(u64::MAX));
            route
                .cooldown(api)
                .fetch_max(available_at, Ordering::Relaxed);
            self.metrics.increment_mojang_errors();
            warn!(?api, ?retry_after, "Mojang rate-limited an outbound route");
            return Err(UpstreamError::RateLimited { retry_after });
        }
        if !status.is_success() {
            return Ok((status, Bytes::new()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_UPSTREAM_RESPONSE_BYTES as u64)
        {
            return Err(self.record_invalid_response("Mojang response exceeded the size limit"));
        }

        let initial_capacity = response
            .content_length()
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(8 * 1024)
            .min(MAX_UPSTREAM_RESPONSE_BYTES);
        let mut body = BytesMut::with_capacity(initial_capacity);
        loop {
            let chunk = response.chunk().await.map_err(|error| {
                self.metrics.record_mojang_response(body.len());
                self.record_transport_error(&error, "failed to read Mojang response")
            })?;
            let Some(chunk) = chunk else {
                break;
            };
            if !try_extend_body(&mut body, &chunk, MAX_UPSTREAM_RESPONSE_BYTES) {
                self.metrics
                    .record_mojang_response(body.len().saturating_add(chunk.len()));
                return Err(self.record_invalid_response("Mojang response exceeded the size limit"));
            }
        }

        self.metrics.record_mojang_response(body.len());
        Ok((status, body.freeze()))
    }

    fn record_transport_error(&self, error: &reqwest::Error, message: &str) -> UpstreamError {
        self.metrics.increment_mojang_errors();
        if error.is_timeout() {
            warn!(error = %ErrorChain(error), "{message}");
            UpstreamError::Timeout
        } else {
            error!(error = %ErrorChain(error), "{message}");
            UpstreamError::Transport
        }
    }

    fn record_status_error(&self, status: StatusCode) -> UpstreamError {
        self.metrics.increment_mojang_errors();
        warn!(%status, "Mojang returned an unsuccessful response");
        UpstreamError::HttpStatus(status.as_u16())
    }

    fn record_invalid_response(&self, error: impl std::fmt::Display) -> UpstreamError {
        self.metrics.increment_mojang_errors();
        error!(%error, "Mojang returned an invalid response");
        UpstreamError::InvalidResponse
    }
}

#[async_trait]
impl MojangService for MojangHttpClient {
    async fn lookup_names(&self, names: &[String]) -> Result<Vec<(String, Uuid)>, UpstreamError> {
        let body = Bytes::from(
            serde_json::to_vec(names).map_err(|error| self.record_invalid_response(error))?,
        );
        let sent_bytes = body.len();
        let endpoint = self
            .endpoints
            .batch_urls
            .get(random_index(self.endpoints.batch_urls.len()))
            .ok_or_else(|| self.record_invalid_response("no Mojang batch endpoint is available"))?;
        let (status, response_body) = self
            .request(Api::Batch, sent_bytes, |client| {
                client
                    .post(endpoint.clone())
                    .header(CONTENT_TYPE, "application/json")
                    .body(body.clone())
            })
            .await?;

        if !status.is_success() {
            return Err(self.record_status_error(status));
        }

        let profiles: Vec<MojangBatchProfile> = serde_json::from_slice(&response_body)
            .map_err(|error| self.record_invalid_response(error))?;
        // One malformed entry should not fail the lookups of every other name in the batch.
        Ok(profiles
            .into_iter()
            .filter_map(|profile| {
                let uuid = parse_minecraft_uuid(&profile.id);
                if uuid.is_none() {
                    warn!(id = ?profile.id, "ignoring Mojang profile with an invalid UUID");
                }
                uuid.map(|uuid| (profile.name, uuid))
            })
            .collect())
    }

    async fn lookup_skin(&self, uuid: Uuid) -> Result<Option<SkinProperty>, UpstreamError> {
        let Ok(_permit) = self.skin_lookups.try_acquire() else {
            warn!("rejecting skin lookup because too many are in progress");
            return Err(UpstreamError::Overloaded);
        };
        let compact_uuid = uuid.simple().to_string();
        let mut endpoint = self
            .endpoints
            .profile_base_url
            .join(&compact_uuid)
            .map_err(|error| self.record_invalid_response(error))?;
        endpoint.set_query(Some("unsigned=false"));

        let (status, response_body) = self
            .request(Api::Profile, 0, |client| client.get(endpoint.clone()))
            .await?;
        if matches!(status, StatusCode::NO_CONTENT | StatusCode::NOT_FOUND) {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(self.record_status_error(status));
        }

        let profile: MojangProfile = serde_json::from_slice(&response_body)
            .map_err(|error| self.record_invalid_response(error))?;
        let Some(textures) = profile
            .properties
            .into_iter()
            .find(|property| property.name == "textures")
        else {
            return Ok(None);
        };
        let signature = textures
            .signature
            .ok_or_else(|| self.record_invalid_response("textures property is not signed"))?;
        Ok(Some(SkinProperty {
            value: textures.value,
            signature,
        }))
    }
}

/// Returns a random index below `len`, or 0 when `len` is 0 or the OS has no randomness.
///
/// The modulo bias does not matter for the few routes and endpoints this picks from.
fn random_index(len: usize) -> usize {
    let mut bytes = [0; size_of::<usize>()];
    if getrandom::fill(&mut bytes).is_err() {
        return 0;
    }
    usize::from_ne_bytes(bytes).checked_rem(len).unwrap_or(0)
}

fn try_extend_body(body: &mut BytesMut, chunk: &[u8], limit: usize) -> bool {
    if body
        .len()
        .checked_add(chunk.len())
        .is_none_or(|length| length > limit)
    {
        return false;
    }
    body.extend_from_slice(chunk);
    true
}

fn load_proxy_list(path: &Path) -> Result<Vec<Url>, ClientBuildError> {
    #[expect(
        clippy::disallowed_methods,
        reason = "The application loads the proxy list during synchronous startup before starting Tokio."
    )]
    let content = fs::read_to_string(path).map_err(|error| ClientBuildError::ReadProxyList {
        path: path.to_path_buf(),
        source: error,
    })?;

    let mut proxies = Vec::new();
    let mut seen = HashSet::new();
    for (line_number, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        match parse_proxy_url(line) {
            Ok(proxy) if seen.insert(proxy.clone()) => proxies.push(proxy),
            Ok(_) => {}
            Err(error) => {
                warn!(
                    path = %path.display(),
                    line = line_number.saturating_add(1),
                    %error,
                    "ignoring invalid proxy entry"
                );
            }
        }
    }
    if proxies.is_empty() {
        warn!(path = %path.display(), "proxy list has no valid entries; using direct connections");
    }
    Ok(proxies)
}

fn parse_proxy_url(value: &str) -> Result<Url, ProxyParseError> {
    let mut parts = value.splitn(4, ':');
    let host = parts.next().filter(|part| !part.is_empty());
    let port = parts.next().filter(|part| !part.is_empty());
    let username = parts.next();
    let password = parts.next();

    let (host, port) = host.zip(port).ok_or(ProxyParseError::Format)?;
    // URL delimiters would move the port into a path or the host into credentials, so
    // `host/x:8080` would otherwise connect to port 80.
    if host.contains(['/', '\\', '?', '#', '@']) {
        return Err(ProxyParseError::Format);
    }
    let port: u16 = port.parse().map_err(ProxyParseError::Port)?;
    let mut url = Url::parse(&format!("http://{host}:{port}")).map_err(ProxyParseError::Url)?;

    match (username, password) {
        (Some(username), Some(password)) if !username.is_empty() => {
            // `Url` leaves `%` unencoded, but reqwest percent-decodes proxy credentials.
            url.set_username(&username.replace('%', "%25"))
                .map_err(|()| ProxyParseError::Credentials)?;
            url.set_password(Some(&password.replace('%', "%25")))
                .map_err(|()| ProxyParseError::Credentials)?;
        }
        (None, None) => {}
        _ => return Err(ProxyParseError::Format),
    }

    Ok(url)
}

#[derive(Debug, Error)]
pub enum ClientBuildError {
    #[error("could not build a Mojang HTTP client")]
    Build(#[from] reqwest::Error),
    #[error("could not read proxy list {path}")]
    ReadProxyList {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Error, PartialEq, Eq)]
enum ProxyParseError {
    #[error("expected host:port or host:port:user:password")]
    Format,
    #[error("proxy port is invalid")]
    Port(#[source] std::num::ParseIntError),
    #[error("expected host:port or host:port:user:password")]
    Url(#[source] url::ParseError),
    #[error("proxy credentials are invalid")]
    Credentials,
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Json, Router,
        extract::{Path, State},
        http::{StatusCode, Uri, header::RETRY_AFTER},
        response::{IntoResponse, Response},
        routing::{get, post},
        serve::ListenerExt,
    };
    use bytes::BytesMut;
    use reqwest::Url;
    use serde_json::json;
    use uuid::Uuid;

    use crate::{metrics::Metrics, types::SkinProperty};

    use super::{
        MojangEndpoints, MojangHttpClient, MojangService, ProxyParseError, UpstreamError,
        parse_proxy_url, try_extend_body,
    };

    const PLAYER: &str = "b1ae07784817436c96a3a72c67cda060";
    const MISSING: &str = "00000000000000000000000000000204";
    const DELETED: &str = "00000000000000000000000000000404";
    const BROKEN: &str = "00000000000000000000000000000500";
    const UNSIGNED: &str = "000000000000000000000000000000aa";

    /// Like Mojang, signs the textures only when the request asks for `unsigned=false`.
    async fn profile(Path(uuid): Path<String>, uri: Uri) -> Response {
        let textures = if uri.query() == Some("unsigned=false") {
            json!({ "name": "textures", "value": "texture", "signature": "signature" })
        } else {
            json!({ "name": "textures", "value": "texture" })
        };
        match uuid.as_str() {
            PLAYER => Json(json!({
                "id": PLAYER,
                "name": "Pistonmaster",
                "properties": [{ "name": "extra", "value": "unsigned" }, textures]
            }))
            .into_response(),
            MISSING => StatusCode::NO_CONTENT.into_response(),
            DELETED => StatusCode::NOT_FOUND.into_response(),
            UNSIGNED => Json(json!({
                "id": UNSIGNED,
                "properties": [{ "name": "textures", "value": "texture" }]
            }))
            .into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }

    /// Answers only for the requested names, which must arrive as a JSON array.
    async fn bulk(Json(names): Json<Vec<String>>) -> Json<serde_json::Value> {
        let profiles = [(PLAYER, "Pistonmaster"), ("not-a-uuid", "Broken")];
        Json(
            profiles
                .into_iter()
                .filter(|(_, profile_name)| {
                    names
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(profile_name))
                })
                .map(|(id, name)| json!({ "id": id, "name": name }))
                .collect(),
        )
    }

    async fn rate_limited(State(hits): State<Arc<AtomicUsize>>) -> Response {
        hits.fetch_add(1, Ordering::SeqCst);
        (StatusCode::TOO_MANY_REQUESTS, [(RETRY_AFTER, "120")]).into_response()
    }

    fn rate_limiting_proxy(retry_after: &'static str) -> Router {
        Router::new().route(
            "/profile/{uuid}",
            get(move || async move {
                (StatusCode::TOO_MANY_REQUESTS, [(RETRY_AFTER, retry_after)])
            }),
        )
    }

    /// Serves `router` and returns its base URL and a count of accepted connections.
    async fn serve(router: Router) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener should bind");
        let address = listener
            .local_addr()
            .expect("fixture should have an address");
        let connections = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&connections);
        let listener = listener.tap_io(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        });
        tokio::spawn(async move {
            axum::serve(listener, router)
                .await
                .expect("fixture server should run");
        });
        (format!("http://{address}"), connections)
    }

    fn client(base: &str, proxies: &[Url]) -> MojangHttpClient {
        MojangHttpClient::build(
            MojangEndpoints {
                batch_urls: vec![Url::parse(&format!("{base}/bulk")).expect("URL should parse")],
                profile_base_url: Url::parse(&format!("{base}/profile/"))
                    .expect("URL should parse"),
            },
            proxies,
            Duration::from_secs(5),
            Arc::new(Metrics::default()),
            false,
        )
        .expect("fixture client should build")
    }

    async fn fixture(router: Router) -> MojangHttpClient {
        let (base, _) = serve(router).await;
        client(&base, &[])
    }

    fn uuid(value: &str) -> Uuid {
        Uuid::parse_str(value).expect("test UUID should parse")
    }

    #[test]
    fn parses_production_endpoints() {
        let endpoints = MojangEndpoints::production();
        assert_eq!(endpoints.batch_urls.len(), 2);
        assert_eq!(
            endpoints.profile_base_url.host_str(),
            Some("sessionserver.mojang.com")
        );
    }

    #[tokio::test]
    async fn reads_skins_and_missing_profiles() {
        let client = fixture(
            Router::new()
                .route("/profile/{uuid}", get(profile))
                .route("/bulk", post(bulk)),
        )
        .await;

        assert_eq!(
            client.lookup_skin(uuid(PLAYER)).await,
            Ok(Some(SkinProperty {
                value: "texture".to_owned(),
                signature: "signature".to_owned(),
            }))
        );
        assert_eq!(client.lookup_skin(uuid(MISSING)).await, Ok(None));
        assert_eq!(client.lookup_skin(uuid(DELETED)).await, Ok(None));
        assert_eq!(
            client.lookup_skin(uuid(BROKEN)).await,
            Err(UpstreamError::HttpStatus(500))
        );
        assert_eq!(
            client.lookup_skin(uuid(UNSIGNED)).await,
            Err(UpstreamError::InvalidResponse)
        );
    }

    #[tokio::test]
    async fn skips_invalid_profiles_in_a_batch() {
        let client = fixture(Router::new().route("/bulk", post(bulk))).await;

        assert_eq!(
            client
                .lookup_names(&["pistonmaster".to_owned(), "broken".to_owned()])
                .await,
            Ok(vec![("Pistonmaster".to_owned(), uuid(PLAYER))])
        );
    }

    #[tokio::test]
    async fn reports_unexpected_batch_statuses() {
        let client = fixture(
            Router::new().route("/bulk", post(|| async { StatusCode::SERVICE_UNAVAILABLE })),
        )
        .await;

        assert_eq!(
            client.lookup_names(&["pistonmaster".to_owned()]).await,
            Err(UpstreamError::HttpStatus(503))
        );
    }

    /// Returns the delay of a rate-limited lookup.
    fn retry_after(result: Result<Option<SkinProperty>, UpstreamError>) -> Duration {
        match result {
            Err(UpstreamError::RateLimited { retry_after }) => retry_after,
            other => panic!("the lookup should be rate-limited: {other:?}"),
        }
    }

    #[tokio::test]
    async fn backs_off_after_mojang_rate_limits_a_route() {
        let hits = Arc::new(AtomicUsize::new(0));
        let client = fixture(
            Router::new()
                .route("/profile/{uuid}", get(rate_limited))
                .route("/bulk", post(bulk))
                .with_state(Arc::clone(&hits)),
        )
        .await;

        // The delay is measured again after the 429, so a millisecond may have passed.
        let delay = retry_after(client.lookup_skin(uuid(PLAYER)).await);
        assert!(
            delay > Duration::from_secs(119) && delay <= Duration::from_secs(120),
            "{delay:?}"
        );
        assert!(
            retry_after(client.lookup_skin(uuid(PLAYER)).await) > Duration::from_secs(100),
            "the cooldown should short-circuit later requests"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let snapshot = client.metrics.snapshot();
        assert_eq!(snapshot.mojang_errors, 1);
        assert_eq!(snapshot.mojang_cooldown_rejections, 1);
        assert_eq!(
            client.lookup_names(&["pistonmaster".to_owned()]).await,
            Ok(vec![("Pistonmaster".to_owned(), uuid(PLAYER))]),
            "Mojang limits each API separately, so name lookups keep working"
        );
    }

    #[tokio::test]
    async fn bounds_the_cooldown_that_mojang_requests() {
        for (header, expected) in [
            ("86400", Duration::from_mins(10)),
            ("0", Duration::from_secs(1)),
            ("soon", Duration::from_secs(30)),
        ] {
            let client = fixture(rate_limiting_proxy(header)).await;
            let delay = retry_after(client.lookup_skin(uuid(PLAYER)).await);
            assert!(
                delay > expected.saturating_sub(Duration::from_secs(1)) && delay <= expected,
                "Retry-After: {header} gave {delay:?}"
            );
        }
    }

    #[tokio::test]
    async fn reports_the_earliest_route_cooldown_after_retrying() {
        let (short, short_connections) = serve(rate_limiting_proxy("30")).await;
        let (long, long_connections) = serve(rate_limiting_proxy("120")).await;
        let proxies = [short, long].map(|base| Url::parse(&base).expect("URL should parse"));
        // Both routes are plain HTTP proxies, so the fixtures see the profile path.
        let client = client("http://mojang.invalid", &proxies);

        assert!(
            matches!(
                client.lookup_skin(uuid(PLAYER)).await,
                Err(UpstreamError::RateLimited { retry_after })
                    if retry_after > Duration::from_secs(25)
                        && retry_after <= Duration::from_secs(30)
            ),
            "the shorter cooldown should decide when to retry"
        );
        assert_eq!(short_connections.load(Ordering::SeqCst), 1);
        assert_eq!(long_connections.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn enforces_response_body_size_while_accumulating_chunks() {
        let mut body = BytesMut::new();

        assert!(try_extend_body(&mut body, b"abc", 3));
        assert!(!try_extend_body(&mut body, b"d", 3));
        assert_eq!(body.as_ref(), b"abc");
    }

    #[test]
    fn parses_direct_and_authenticated_proxy_entries() {
        let direct = parse_proxy_url("127.0.0.1:8080").expect("direct proxy should parse");
        assert_eq!(direct.as_str(), "http://127.0.0.1:8080/");

        let authenticated =
            parse_proxy_url("proxy.example:3128:user:pass:word").expect("proxy should parse");
        assert_eq!(authenticated.username(), "user");
        assert_eq!(
            authenticated.as_str(),
            "http://user:pass%3Aword@proxy.example:3128/"
        );

        // Reqwest percent-decodes proxy credentials, so a literal `%` must stay encoded.
        let percent = parse_proxy_url("proxy.example:3128:us%er:p%41ss").expect("should parse");
        assert_eq!(percent.username(), "us%25er");
        assert_eq!(percent.password(), Some("p%2541ss"));
    }

    #[test]
    fn rejects_incomplete_proxy_entries() {
        assert_eq!(parse_proxy_url("127.0.0.1"), Err(ProxyParseError::Format));
        assert!(matches!(
            parse_proxy_url("127.0.0.1:not-a-port"),
            Err(ProxyParseError::Port(_))
        ));
        assert!(matches!(
            parse_proxy_url("bad host:8080"),
            Err(ProxyParseError::Url(_))
        ));
        assert_eq!(
            parse_proxy_url("127.0.0.1:8080:user"),
            Err(ProxyParseError::Format)
        );
        for entry in [
            "proxy.example/path:8080",
            "proxy.example\\path:8080",
            "proxy.example?x:8080",
            "proxy.example#x:8080",
            "user@proxy.example:8080",
        ] {
            assert_eq!(
                parse_proxy_url(entry),
                Err(ProxyParseError::Format),
                "{entry} should be rejected"
            );
        }
    }
}
