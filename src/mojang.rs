#![warn(clippy::indexing_slicing, clippy::arithmetic_side_effects)]

use std::{
    collections::HashSet,
    fs,
    num::NonZeroU16,
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
    log_throttle::{LOG_INTERVAL, LogThrottle},
    metrics::Metrics,
    types::{MojangBatchProfile, MojangProfile, SkinProperty},
    validation::parse_minecraft_uuid,
};

const MAX_UPSTREAM_RESPONSE_BYTES: usize = 1024 * 1024;
/// Bounds one Mojang request. The `SkinsRestorer` plugin gives up after 5 seconds, so a slower
/// answer is unlikely to reach a player anyway.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(4);
/// Bounds all attempts of one Mojang call, including a retry after `429`. The batch lookup
/// deadline in `batch.rs` and the router deadline in `app.rs` build on this value.
pub(crate) const UPSTREAM_BUDGET: Duration = Duration::from_secs(5);
/// A retry needs at least this much of [`UPSTREAM_BUDGET`] left to have a fair chance.
const MIN_RETRY_TIMEOUT: Duration = Duration::from_secs(2);
/// Limits concurrent session server requests, so random UUIDs cannot flood Mojang.
const MAX_CONCURRENT_SKIN_LOOKUPS: usize = 64;
const DEFAULT_RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(30);
const MAX_RATE_LIMIT_COOLDOWN: Duration = Duration::from_mins(10);
/// How long a route that could not connect receives no traffic while other routes work.
const UNREACHABLE_ROUTE_COOLDOWN: Duration = Duration::from_secs(30);
/// A rate-limited or unreachable request is retried once through another outbound route, if
/// [`retry_timeout`] allows it.
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
    /// The batch processor stopped, so this service cannot run the lookup.
    #[error("the batch processor has stopped")]
    BatchStopped,
}

#[async_trait]
pub trait MojangService: Send + Sync {
    async fn lookup_names(&self, names: &[String]) -> Result<Vec<(String, Uuid)>, UpstreamError>;

    async fn lookup_skin(&self, uuid: Uuid) -> Result<Option<SkinProperty>, UpstreamError>;
}

/// One outbound path to Mojang: a direct connection or a single proxy.
///
/// Each route has its own client because pooled connections stay bound to the proxy that
/// opened them. Logs name a route by its index (the direct connection or the proxy's
/// position among the valid entries), never by its URL, which can contain credentials.
#[derive(Debug)]
struct Route {
    client: Client,
    /// Mojang rate-limits each API host separately. For each upstream (see
    /// `MojangEndpoints::upstream_url`), milliseconds after `MojangHttpClient::epoch` until
    /// Mojang accepts requests from this route again.
    rate_limited_until: Vec<AtomicU64>,
    /// Milliseconds after the epoch until the route gets traffic again after a failed connection.
    unreachable_until: AtomicU64,
}

impl Route {
    fn is_rate_limited(&self, upstream: usize, now: u64) -> bool {
        self.rate_limited_until
            .get(upstream)
            .is_none_or(|until| until.load(Ordering::Relaxed) > now)
    }
}

/// The Mojang endpoints, each on its own host. Upstream `i` is batch endpoint `i`, and the
/// upstream after the batch endpoints is the profile endpoint.
#[derive(Debug)]
struct MojangEndpoints {
    batch_urls: Vec<Url>,
    profile_base_url: Url,
}

impl MojangEndpoints {
    fn batch_upstreams(&self) -> Vec<usize> {
        (0..self.batch_urls.len()).collect()
    }

    const fn profile_upstream(&self) -> usize {
        self.batch_urls.len()
    }

    fn upstream_url(&self, upstream: usize) -> Option<&Url> {
        self.batch_urls
            .get(upstream)
            .or_else(|| (upstream == self.profile_upstream()).then_some(&self.profile_base_url))
    }
}

/// Why a single attempt through one route failed.
enum AttemptError {
    /// The route could not connect, so the request never reached Mojang.
    Unreachable,
    Upstream(UpstreamError),
}

impl From<UpstreamError> for AttemptError {
    fn from(error: UpstreamError) -> Self {
        Self::Upstream(error)
    }
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
        let upstream_count = endpoints.profile_upstream().saturating_add(1);
        let route = |client| Route {
            client,
            rate_limited_until: (0..upstream_count).map(|_| AtomicU64::new(0)).collect(),
            unreachable_until: AtomicU64::new(0),
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

    fn millis_from_now(&self, delay: Duration) -> u64 {
        self.now_millis()
            .saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
    }

    /// Picks a random route and one of `upstreams` that Mojang has not rate-limited on it.
    ///
    /// Routes that recently failed to connect are skipped. When `allow_unreachable` is set and
    /// no other route is ready, they are used anyway, so a short network outage does not block
    /// every request until the cooldown ends.
    fn choose_route(
        &self,
        upstreams: &[usize],
        allow_unreachable: bool,
    ) -> Option<(usize, &Route, usize)> {
        let now = self.now_millis();
        let start = random_index(self.routes.len());
        let find = |skip_unreachable: bool| {
            self.routes
                .iter()
                .enumerate()
                .cycle()
                .skip(start)
                .take(self.routes.len())
                .find_map(|(index, route)| {
                    if skip_unreachable && route.unreachable_until.load(Ordering::Relaxed) > now {
                        return None;
                    }
                    let ready: Vec<usize> = upstreams
                        .iter()
                        .copied()
                        .filter(|&upstream| !route.is_rate_limited(upstream, now))
                        .collect();
                    ready
                        .get(random_index(ready.len()))
                        .map(|&upstream| (index, route, upstream))
                })
        };
        find(true).or_else(|| if allow_unreachable { find(false) } else { None })
    }

    /// Reports how long until the first route becomes available for one of `upstreams` again.
    fn rate_limited(&self, upstreams: &[usize]) -> UpstreamError {
        let now = self.now_millis();
        let available_at = self
            .routes
            .iter()
            .flat_map(|route| {
                upstreams.iter().filter_map(|&upstream| {
                    route
                        .rate_limited_until
                        .get(upstream)
                        .map(|until| until.load(Ordering::Relaxed))
                })
            })
            .min()
            .unwrap_or(now);
        UpstreamError::RateLimited {
            retry_after: Duration::from_millis(available_at.saturating_sub(now)),
        }
    }

    /// Sends a request to one of `upstreams`. `build` receives the route's client and the
    /// upstream's URL.
    async fn request(
        &self,
        upstreams: &[usize],
        sent_bytes: usize,
        build: impl Fn(&Client, &Url) -> RequestBuilder,
    ) -> Result<(StatusCode, Bytes), UpstreamError> {
        let started = tokio::time::Instant::now();
        let mut retried_error = None;
        for attempt in 0..MAX_ATTEMPTS {
            let request_timeout = if attempt > 0 {
                let Some(timeout) = retry_timeout(started.elapsed()) else {
                    break;
                };
                Some(timeout)
            } else {
                None
            };
            // A retry needs another route that is not cooling down.
            let Some((index, route, upstream)) =
                self.choose_route(upstreams, retried_error.is_none())
            else {
                break;
            };
            let url = self
                .endpoints
                .upstream_url(upstream)
                .ok_or_else(|| self.record_invalid_response("unknown Mojang endpoint"))?;
            let mut request = build(&route.client, url);
            if let Some(timeout) = request_timeout {
                request = request.timeout(timeout);
            }
            match self.send(route, index, upstream, request, sent_bytes).await {
                Ok(response) => return Ok(response),
                Err(AttemptError::Unreachable) => retried_error = Some(UpstreamError::Transport),
                Err(AttemptError::Upstream(error @ UpstreamError::RateLimited { .. })) => {
                    retried_error = Some(error);
                }
                Err(AttemptError::Upstream(error)) => return Err(error),
            }
        }
        match retried_error {
            Some(UpstreamError::Transport) => Err(UpstreamError::Transport),
            // The last route's cooldown can outlast one that an earlier attempt rate-limited.
            Some(_) => Err(self.rate_limited(upstreams)),
            None => {
                // Failed attempts already counted as errors.
                self.metrics.increment_mojang_cooldown_rejections();
                let error = self.rate_limited(upstreams);
                static ALL_ROUTES_COOLING_DOWN: LogThrottle = LogThrottle::new(LOG_INTERVAL);
                if let Some(suppressed) = ALL_ROUTES_COOLING_DOWN.check() {
                    warn!(
                        ?upstreams,
                        ?error,
                        suppressed,
                        "rejecting Mojang request because every outbound route is cooling down"
                    );
                }
                Err(error)
            }
        }
    }

    /// Sends a request through `route`, the route at `index`, and reads the body of successful responses.
    ///
    /// Unsuccessful responses return their status with an empty body.
    async fn send(
        &self,
        route: &Route,
        index: usize,
        upstream: usize,
        request: RequestBuilder,
        sent_bytes: usize,
    ) -> Result<(StatusCode, Bytes), AttemptError> {
        let host = self
            .endpoints
            .upstream_url(upstream)
            .and_then(Url::host_str)
            .unwrap_or_default();
        self.metrics.record_mojang_request(sent_bytes);

        let mut response = match request.send().await {
            Ok(response) => response,
            Err(error) if error.is_connect() && !error.is_timeout() => {
                self.metrics.increment_mojang_errors();
                route.unreachable_until.fetch_max(
                    self.millis_from_now(UNREACHABLE_ROUTE_COOLDOWN),
                    Ordering::Relaxed,
                );
                error!(
                    route = index,
                    host,
                    error = %ErrorChain(&error),
                    "could not connect to Mojang through an outbound route"
                );
                return Err(AttemptError::Unreachable);
            }
            Err(error) => {
                return Err(self
                    .record_transport_error(&error, index, "Mojang request failed")
                    .into());
            }
        };
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse().ok())
                .map_or(DEFAULT_RATE_LIMIT_COOLDOWN, Duration::from_secs)
                .clamp(Duration::from_secs(1), MAX_RATE_LIMIT_COOLDOWN);
            if let Some(until) = route.rate_limited_until.get(upstream) {
                until.fetch_max(self.millis_from_now(retry_after), Ordering::Relaxed);
            }
            self.metrics.increment_mojang_errors();
            warn!(
                route = index,
                host,
                ?retry_after,
                "Mojang rate-limited an outbound route"
            );
            return Err(UpstreamError::RateLimited { retry_after }.into());
        }
        if !status.is_success() {
            return Ok((status, Bytes::new()));
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_UPSTREAM_RESPONSE_BYTES as u64)
        {
            return Err(self
                .record_invalid_response("Mojang response exceeded the size limit")
                .into());
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
                self.record_transport_error(&error, index, "failed to read Mojang response")
            })?;
            let Some(chunk) = chunk else {
                break;
            };
            if !try_extend_body(&mut body, &chunk, MAX_UPSTREAM_RESPONSE_BYTES) {
                self.metrics
                    .record_mojang_response(body.len().saturating_add(chunk.len()));
                return Err(self
                    .record_invalid_response("Mojang response exceeded the size limit")
                    .into());
            }
        }

        self.metrics.record_mojang_response(body.len());
        Ok((status, body.freeze()))
    }

    fn record_transport_error(
        &self,
        error: &reqwest::Error,
        route: usize,
        message: &str,
    ) -> UpstreamError {
        self.metrics.increment_mojang_errors();
        if error.is_timeout() {
            warn!(route, error = %ErrorChain(error), "{message}");
            UpstreamError::Timeout
        } else {
            error!(route, error = %ErrorChain(error), "{message}");
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
        let (status, response_body) = self
            .request(
                &self.endpoints.batch_upstreams(),
                sent_bytes,
                |client, url| {
                    client
                        .post(url.clone())
                        .header(CONTENT_TYPE, "application/json")
                        .body(body.clone())
                },
            )
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
            self.metrics.increment_skin_lookup_rejections();
            static TOO_MANY_SKIN_LOOKUPS: LogThrottle = LogThrottle::new(LOG_INTERVAL);
            if let Some(suppressed) = TOO_MANY_SKIN_LOOKUPS.check() {
                warn!(
                    suppressed,
                    "rejecting skin lookup because too many are in progress"
                );
            }
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
            // The profile URL also names the UUID, so the base URL is not used.
            .request(&[self.endpoints.profile_upstream()], 0, |client, _| {
                client.get(endpoint.clone())
            })
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

/// Returns the timeout for a retry that starts `elapsed` after the first attempt, or `None` when
/// too little of [`UPSTREAM_BUDGET`] remains.
fn retry_timeout(elapsed: Duration) -> Option<Duration> {
    let remaining = UPSTREAM_BUDGET.saturating_sub(elapsed);
    (remaining >= MIN_RETRY_TIMEOUT).then(|| remaining.min(REQUEST_TIMEOUT))
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
    let value = value
        .get(..7)
        .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
        .and_then(|_| value.get(7..))
        .unwrap_or(value);
    // An IPv6 host contains colons, so it must be bracketed: `[::1]:8080`.
    let (host, rest) = if let Some(value) = value.strip_prefix('[') {
        let (address, rest) = value.split_once(']').ok_or(ProxyParseError::Format)?;
        (
            format!("[{address}]"),
            rest.strip_prefix(':').ok_or(ProxyParseError::Format)?,
        )
    } else {
        let (host, rest) = value.split_once(':').ok_or(ProxyParseError::Format)?;
        (host.to_owned(), rest)
    };
    let mut parts = rest.splitn(3, ':');
    let port = parts.next().filter(|part| !part.is_empty());
    let username = parts.next();
    let password = parts.next();

    let port = port.ok_or(ProxyParseError::Format)?;
    // URL delimiters would move the port into a path or the host into credentials, so
    // `host/x:8080` would otherwise connect to port 80.
    if host.is_empty() || host.contains(['/', '\\', '?', '#', '@']) {
        return Err(ProxyParseError::Format);
    }
    // Port 0 cannot accept connections.
    let port: NonZeroU16 = port.parse().map_err(ProxyParseError::Port)?;
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
        parse_proxy_url, retry_timeout, try_extend_body,
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
        client_with_batch_paths(base, &["bulk"], proxies)
    }

    fn client_with_batch_paths(
        base: &str,
        batch_paths: &[&str],
        proxies: &[Url],
    ) -> MojangHttpClient {
        MojangHttpClient::build(
            MojangEndpoints {
                batch_urls: batch_paths
                    .iter()
                    .map(|path| Url::parse(&format!("{base}/{path}")).expect("URL should parse"))
                    .collect(),
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
    async fn rejects_and_counts_skin_lookups_beyond_the_concurrency_limit() {
        let client = client("http://127.0.0.1:9", &[]);
        let _busy = client
            .skin_lookups
            .try_acquire_many(u32::try_from(super::MAX_CONCURRENT_SKIN_LOOKUPS).unwrap_or(u32::MAX))
            .expect("the test should take every skin lookup permit");

        assert_eq!(
            client.lookup_skin(uuid(PLAYER)).await,
            Err(UpstreamError::Overloaded)
        );
        let snapshot = client.metrics.snapshot();
        assert_eq!(snapshot.skin_lookup_rejections, 1);
        assert_eq!(snapshot.mojang_requests, 0);
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

    #[tokio::test]
    async fn keeps_using_the_batch_host_that_mojang_did_not_rate_limit() {
        let hits = Arc::new(AtomicUsize::new(0));
        let (base, _) = serve(
            Router::new()
                .route("/limited", post(rate_limited))
                .route("/bulk", post(bulk))
                .with_state(Arc::clone(&hits)),
        )
        .await;
        let client = client_with_batch_paths(&base, &["limited", "bulk"], &[]);

        // Each lookup picks a batch host at random until one host is rate-limited.
        for _ in 0..50 {
            assert_eq!(
                client.lookup_names(&["pistonmaster".to_owned()]).await,
                Ok(vec![("Pistonmaster".to_owned(), uuid(PLAYER))]),
                "a 429 from one batch host should not pause the other"
            );
        }
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let snapshot = client.metrics.snapshot();
        assert_eq!(snapshot.mojang_errors, 1);
        assert_eq!(snapshot.mojang_cooldown_rejections, 0);
    }

    /// Returns the URL of a local port that refuses connections.
    async fn closed_port() -> Url {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let address = listener
            .local_addr()
            .expect("listener should have an address");
        drop(listener);
        Url::parse(&format!("http://{address}")).expect("URL should parse")
    }

    #[tokio::test]
    async fn stops_sending_requests_through_an_unreachable_proxy() {
        let (live, live_connections) =
            serve(Router::new().route("/profile/{uuid}", get(profile))).await;
        let proxies = [
            closed_port().await,
            Url::parse(&live).expect("URL should parse"),
        ];
        let client = client("http://mojang.invalid", &proxies);

        // Each lookup picks a proxy at random until the closed one fails once.
        for _ in 0..50 {
            assert!(
                matches!(client.lookup_skin(uuid(PLAYER)).await, Ok(Some(_))),
                "a connection failure should be retried through the other proxy"
            );
        }
        assert_eq!(
            client.metrics.snapshot().mojang_errors,
            1,
            "the unreachable proxy should get no traffic during its cooldown"
        );
        assert!(live_connections.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn keeps_trying_the_only_route_after_a_connection_failure() {
        let base = closed_port().await;
        let client = client(base.as_str().trim_end_matches('/'), &[]);

        for _ in 0..2 {
            assert_eq!(
                client.lookup_skin(uuid(PLAYER)).await,
                Err(UpstreamError::Transport)
            );
        }
        let snapshot = client.metrics.snapshot();
        assert_eq!(snapshot.mojang_errors, 2);
        assert_eq!(snapshot.mojang_cooldown_rejections, 0);
    }

    #[test]
    fn gives_a_retry_only_the_rest_of_the_budget() {
        let secs = Duration::from_secs_f32;
        assert_eq!(retry_timeout(Duration::ZERO), Some(secs(4.0)));
        assert_eq!(retry_timeout(secs(1.0)), Some(secs(4.0)));
        assert_eq!(retry_timeout(secs(2.5)), Some(secs(2.5)));
        assert_eq!(retry_timeout(secs(3.0)), Some(secs(2.0)));
        assert_eq!(retry_timeout(secs(3.5)), None);
        assert_eq!(retry_timeout(secs(60.0)), None);
    }

    // The paused clock skips the slow responses, so the test takes no real time.
    #[tokio::test(start_paused = true)]
    async fn skips_the_retry_when_a_slow_rate_limit_used_up_the_budget() {
        let slow_rate_limit = || {
            Router::new().route(
                "/profile/{uuid}",
                get(|| async {
                    tokio::time::sleep(Duration::from_millis(3_500)).await;
                    (StatusCode::TOO_MANY_REQUESTS, [(RETRY_AFTER, "30")])
                }),
            )
        };
        let (first, first_connections) = serve(slow_rate_limit()).await;
        let (second, second_connections) = serve(slow_rate_limit()).await;
        let proxies = [first, second].map(|base| Url::parse(&base).expect("URL should parse"));
        let client = client("http://mojang.invalid", &proxies);

        assert!(matches!(
            client.lookup_skin(uuid(PLAYER)).await,
            Err(UpstreamError::RateLimited { .. })
        ));
        assert_eq!(
            first_connections.load(Ordering::SeqCst) + second_connections.load(Ordering::SeqCst),
            1,
            "a retry would not finish within the budget"
        );
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
    fn parses_prefixed_and_ipv6_proxy_entries() {
        for (entry, expected) in [
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080/"),
            (
                "HTTP://proxy.example:3128:user:pass",
                "http://user:pass@proxy.example:3128/",
            ),
            ("[::1]:8080", "http://[::1]:8080/"),
            (
                "http://[::1]:8080:user:pass",
                "http://user:pass@[::1]:8080/",
            ),
        ] {
            let proxy = parse_proxy_url(entry).expect("proxy should parse");
            assert_eq!(proxy.as_str(), expected, "{entry}");
        }
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
        assert!(matches!(
            parse_proxy_url("127.0.0.1:0"),
            Err(ProxyParseError::Port(_))
        ));
        assert!(
            parse_proxy_url("https://127.0.0.1:8080").is_err(),
            "only HTTP proxies are supported"
        );
        for entry in [
            "::1:8080",
            "[::1]",
            "[::1]8080",
            "[::1:8080",
            ":8080",
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
