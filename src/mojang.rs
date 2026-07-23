use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use rand::seq::IndexedRandom;
use reqwest::{
    Client, Proxy, StatusCode, Url,
    header::{ACCEPT, ACCEPT_LANGUAGE, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT},
};
use thiserror::Error;
use tracing::{error, warn};
use uuid::Uuid;

use crate::{
    config::MojangEndpoints,
    metrics::Metrics,
    types::{MojangBatchProfile, MojangProfile, SkinProperty},
    validation::parse_minecraft_uuid,
};

const MAX_UPSTREAM_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UpstreamError {
    #[error("the Mojang request timed out")]
    Timeout,
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

#[derive(Debug, Clone)]
pub struct MojangHttpClient {
    client: Client,
    endpoints: MojangEndpoints,
    metrics: Arc<Metrics>,
}

impl MojangHttpClient {
    /// Creates the shared Mojang HTTP client and loads any configured proxies.
    ///
    /// # Errors
    ///
    /// Returns an error when the proxy list cannot be read or the HTTP client cannot be built.
    pub fn new(
        endpoints: MojangEndpoints,
        proxy_list_file: Option<&Path>,
        timeout: Duration,
        metrics: Arc<Metrics>,
    ) -> Result<Self, ClientBuildError> {
        if endpoints.batch_urls.is_empty() {
            return Err(ClientBuildError::NoBatchEndpoints);
        }

        let mut default_headers = HeaderMap::new();
        default_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        default_headers.insert(ACCEPT_LANGUAGE, HeaderValue::from_static("en-US,en"));
        default_headers.insert(USER_AGENT, HeaderValue::from_static("SRMojangAPI"));

        let proxies = proxy_list_file
            .map(load_proxy_list)
            .transpose()?
            .unwrap_or_default();
        let mut builder = Client::builder()
            .default_headers(default_headers)
            .timeout(timeout)
            .https_only(true);

        if !proxies.is_empty() {
            let proxies = Arc::new(proxies);
            let proxy_count = proxies.len();
            builder = builder.proxy(Proxy::custom(move |_| {
                proxies.choose(&mut rand::rng()).cloned()
            }));
            tracing::info!(proxy_count, "loaded outbound proxies");
        }

        Ok(Self {
            client: builder.build()?,
            endpoints,
            metrics,
        })
    }

    async fn send(
        &self,
        request: reqwest::RequestBuilder,
        sent_bytes: usize,
    ) -> Result<(StatusCode, Bytes), UpstreamError> {
        self.metrics.record_mojang_request(sent_bytes);

        let mut response = request.send().await.map_err(|error| {
            self.metrics.increment_mojang_errors();
            if error.is_timeout() {
                UpstreamError::Timeout
            } else {
                tracing::error!(%error, "Mojang request failed");
                UpstreamError::Transport
            }
        })?;
        let status = response.status();
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
                self.metrics.increment_mojang_errors();
                if error.is_timeout() {
                    UpstreamError::Timeout
                } else {
                    tracing::error!(%error, "failed to read Mojang response");
                    UpstreamError::Transport
                }
            })?;
            let Some(chunk) = chunk else {
                break;
            };
            if !try_extend_body(&mut body, &chunk, MAX_UPSTREAM_RESPONSE_BYTES) {
                return Err(self.record_invalid_response("Mojang response exceeded the size limit"));
            }
        }

        self.metrics.record_mojang_response(body.len());
        Ok((status, body.freeze()))
    }

    fn record_status_error(&self, status: StatusCode) -> UpstreamError {
        self.metrics.increment_mojang_errors();
        error!(%status, "Mojang returned an unsuccessful response");
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
        let body =
            serde_json::to_vec(names).map_err(|error| self.record_invalid_response(error))?;
        let sent_bytes = body.len();
        let endpoint = self
            .endpoints
            .batch_urls
            .choose(&mut rand::rng())
            .ok_or_else(|| self.record_invalid_response("no Mojang batch endpoint is available"))?;
        let request = self
            .client
            .post(endpoint.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(body);
        let (status, response_body) = self.send(request, sent_bytes).await?;

        if !status.is_success() {
            return Err(self.record_status_error(status));
        }

        let profiles: Vec<MojangBatchProfile> = serde_json::from_slice(&response_body)
            .map_err(|error| self.record_invalid_response(error))?;
        profiles
            .into_iter()
            .map(|profile| {
                parse_minecraft_uuid(&profile.id)
                    .map(|uuid| (profile.name, uuid))
                    .ok_or_else(|| {
                        self.record_invalid_response("profile contained an invalid UUID")
                    })
            })
            .collect()
    }

    async fn lookup_skin(&self, uuid: Uuid) -> Result<Option<SkinProperty>, UpstreamError> {
        let compact_uuid = uuid.simple().to_string();
        let mut endpoint = self
            .endpoints
            .profile_base_url
            .join(&compact_uuid)
            .map_err(|error| self.record_invalid_response(error))?;
        endpoint.set_query(Some("unsigned=false"));

        let (status, response_body) = self.send(self.client.get(endpoint), 0).await?;
        if status == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(self.record_status_error(status));
        }

        let profile: MojangProfile = serde_json::from_slice(&response_body)
            .map_err(|error| self.record_invalid_response(error))?;
        Ok(profile
            .properties
            .into_iter()
            .find(|property| property.name == "textures")
            .map(|property| SkinProperty {
                value: property.value,
                signature: property.signature,
            }))
    }
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
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            warn!(path = %path.display(), "proxy list file does not exist; using direct connections");
            return Ok(Vec::new());
        }
        Err(error) => {
            return Err(ClientBuildError::ReadProxyList {
                path: path.to_path_buf(),
                source: error,
            });
        }
    };

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
    Ok(proxies)
}

fn parse_proxy_url(value: &str) -> Result<Url, ProxyParseError> {
    let mut parts = value.splitn(4, ':');
    let host = parts.next().filter(|part| !part.is_empty());
    let port = parts.next().filter(|part| !part.is_empty());
    let username = parts.next();
    let password = parts.next();

    let (host, port) = host.zip(port).ok_or(ProxyParseError::Format)?;
    let port: u16 = port.parse().map_err(|_| ProxyParseError::Port)?;
    let mut url =
        Url::parse(&format!("http://{host}:{port}")).map_err(|_| ProxyParseError::Format)?;

    match (username, password) {
        (Some(username), Some(password)) if !username.is_empty() => {
            url.set_username(username)
                .map_err(|()| ProxyParseError::Credentials)?;
            url.set_password(Some(password))
                .map_err(|()| ProxyParseError::Credentials)?;
        }
        (None, None) => {}
        _ => return Err(ProxyParseError::Format),
    }

    Ok(url)
}

#[derive(Debug, Error)]
pub enum ClientBuildError {
    #[error("at least one Mojang batch endpoint is required")]
    NoBatchEndpoints,
    #[error("could not build the Mojang HTTP client")]
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
    #[error("expected ip:port or ip:port:user:password")]
    Format,
    #[error("proxy port is invalid")]
    Port,
    #[error("proxy credentials are invalid")]
    Credentials,
}

#[cfg(test)]
mod tests {
    use bytes::BytesMut;

    use super::{ProxyParseError, parse_proxy_url, try_extend_body};

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
    }

    #[test]
    fn rejects_incomplete_proxy_entries() {
        assert_eq!(parse_proxy_url("127.0.0.1"), Err(ProxyParseError::Format));
        assert_eq!(
            parse_proxy_url("127.0.0.1:not-a-port"),
            Err(ProxyParseError::Port)
        );
        assert_eq!(
            parse_proxy_url("127.0.0.1:8080:user"),
            Err(ProxyParseError::Format)
        );
    }
}
