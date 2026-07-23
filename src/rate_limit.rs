use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    net::{IpAddr, Ipv6Addr},
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{Request, State},
    http::{
        HeaderName, HeaderValue, StatusCode,
        header::{RETRY_AFTER, X_CONTENT_TYPE_OPTIONS},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use thiserror::Error;

use crate::types::RateLimitResponse;

const CF_CONNECTING_IP: HeaderName = HeaderName::from_static("cf-connecting-ip");
const RATE_LIMIT_LIMIT: HeaderName = HeaderName::from_static("ratelimit-limit");
const RATE_LIMIT_POLICY: HeaderName = HeaderName::from_static("ratelimit-policy");
const RATE_LIMIT_REMAINING: HeaderName = HeaderName::from_static("ratelimit-remaining");
const RATE_LIMIT_RESET: HeaderName = HeaderName::from_static("ratelimit-reset");
const RATE_LIMIT_SHARDS: usize = 32;

#[derive(Debug)]
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    shards: Box<[Mutex<RateLimiterState>]>,
    limit_header: HeaderValue,
    policy_header: HeaderValue,
}

#[derive(Debug)]
struct RateLimiterState {
    clients: HashMap<IpAddr, ClientWindow>,
    last_cleanup: Instant,
}

#[derive(Debug, Clone, Copy)]
struct ClientWindow {
    started_at: Instant,
    requests: u32,
}

#[derive(Debug, Clone, Copy)]
struct Decision {
    allowed: bool,
    remaining: u32,
    reset_after: Duration,
}

impl RateLimiter {
    /// Creates an in-memory fixed-window rate limiter.
    ///
    /// # Errors
    ///
    /// Returns an error when the limit or window is zero.
    pub fn new(limit: u32, window: Duration) -> Result<Self, RateLimitConfigError> {
        if limit == 0 {
            return Err(RateLimitConfigError::ZeroLimit);
        }
        if window.is_zero() {
            return Err(RateLimitConfigError::ZeroWindow);
        }
        let policy_header = HeaderValue::from_str(&format!("{limit};w={}", window.as_secs()))
            .map_err(|_| RateLimitConfigError::InvalidPolicyHeader)?;
        let now = Instant::now();
        Ok(Self {
            limit,
            window,
            shards: (0..RATE_LIMIT_SHARDS)
                .map(|_| {
                    Mutex::new(RateLimiterState {
                        clients: HashMap::new(),
                        last_cleanup: now,
                    })
                })
                .collect(),
            limit_header: HeaderValue::from(limit),
            policy_header,
        })
    }

    fn check(&self, key: IpAddr, now: Instant) -> Decision {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let shard_index =
            usize::try_from(hasher.finish()).unwrap_or(usize::MAX) % self.shards.len();
        let mut state = self.shards[shard_index]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.saturating_duration_since(state.last_cleanup) >= self.window {
            state
                .clients
                .retain(|_, client| now.saturating_duration_since(client.started_at) < self.window);
            state.last_cleanup = now;
        }
        let client = state.clients.entry(key).or_insert(ClientWindow {
            started_at: now,
            requests: 0,
        });
        let elapsed = now.saturating_duration_since(client.started_at);
        if elapsed >= self.window {
            *client = ClientWindow {
                started_at: now,
                requests: 0,
            };
        }

        let allowed = client.requests < self.limit;
        if allowed {
            client.requests = client.requests.saturating_add(1);
        }
        Decision {
            allowed,
            remaining: self.limit.saturating_sub(client.requests),
            reset_after: self
                .window
                .saturating_sub(now.saturating_duration_since(client.started_at)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RateLimitConfigError {
    #[error("rate limit must be greater than zero")]
    ZeroLimit,
    #[error("rate limit window must not be zero")]
    ZeroWindow,
    #[error("rate limit policy could not be represented as an HTTP header")]
    InvalidPolicyHeader,
}

pub async fn enforce_rate_limit(
    State(rate_limiter): State<std::sync::Arc<RateLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    let key = request
        .headers()
        .get(&CF_CONNECTING_IP)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .unwrap_or(IpAddr::V6(Ipv6Addr::UNSPECIFIED));
    let decision = rate_limiter.check(key, Instant::now());
    let reset_seconds = decision.reset_after.as_secs().max(1);
    let reset_header = HeaderValue::from(reset_seconds);

    let mut response = if decision.allowed {
        next.run(request).await
    } else {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(RateLimitResponse {
                error: "Too Many Requests",
            }),
        )
            .into_response()
    };
    let headers = response.headers_mut();
    headers.insert(RATE_LIMIT_LIMIT, rate_limiter.limit_header.clone());
    headers.insert(RATE_LIMIT_POLICY, rate_limiter.policy_header.clone());
    headers.insert(RATE_LIMIT_REMAINING, HeaderValue::from(decision.remaining));
    headers.insert(RATE_LIMIT_RESET, reset_header.clone());
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if !decision.allowed {
        headers.insert(RETRY_AFTER, reset_header);
    }
    response
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        time::{Duration, Instant},
    };

    use super::{RateLimitConfigError, RateLimiter};

    #[test]
    fn rejects_zero_rate_limit_configuration_values() {
        assert!(matches!(
            RateLimiter::new(0, Duration::from_secs(1)),
            Err(RateLimitConfigError::ZeroLimit)
        ));
        assert!(matches!(
            RateLimiter::new(1, Duration::ZERO),
            Err(RateLimitConfigError::ZeroWindow)
        ));
    }

    #[test]
    fn enforces_independent_fixed_windows() {
        let limiter = RateLimiter::new(2, Duration::from_mins(1))
            .expect("test rate limiter configuration should be valid");
        let now = Instant::now();
        let first = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let second = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));

        assert!(limiter.check(first, now).allowed);
        assert!(limiter.check(first, now).allowed);
        assert!(!limiter.check(first, now).allowed);
        assert!(limiter.check(second, now).allowed);
        assert!(limiter.check(first, now + Duration::from_mins(1)).allowed);
    }
}
