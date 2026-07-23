use std::{
    collections::HashMap,
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

#[derive(Debug)]
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    state: Mutex<RateLimiterState>,
}

#[derive(Debug)]
struct RateLimiterState {
    clients: HashMap<String, ClientWindow>,
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
        Ok(Self {
            limit,
            window,
            state: Mutex::new(RateLimiterState {
                clients: HashMap::new(),
                last_cleanup: Instant::now(),
            }),
        })
    }

    fn check(&self, key: &str, now: Instant) -> Decision {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.saturating_duration_since(state.last_cleanup) >= self.window {
            state
                .clients
                .retain(|_, client| now.saturating_duration_since(client.started_at) < self.window);
            state.last_cleanup = now;
        }
        let client = state.clients.entry(key.to_owned()).or_insert(ClientWindow {
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
        .unwrap_or_default();
    let decision = rate_limiter.check(key, Instant::now());
    let reset_seconds = decision.reset_after.as_secs().max(1);

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
    headers.insert(
        RATE_LIMIT_LIMIT,
        HeaderValue::from_str(&rate_limiter.limit.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("1000")),
    );
    headers.insert(
        RATE_LIMIT_POLICY,
        HeaderValue::from_str(&format!(
            "{};w={}",
            rate_limiter.limit,
            rate_limiter.window.as_secs()
        ))
        .unwrap_or_else(|_| HeaderValue::from_static("1000;w=60")),
    );
    headers.insert(
        RATE_LIMIT_REMAINING,
        HeaderValue::from_str(&decision.remaining.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("0")),
    );
    headers.insert(
        RATE_LIMIT_RESET,
        HeaderValue::from_str(&reset_seconds.to_string())
            .unwrap_or_else(|_| HeaderValue::from_static("1")),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if !decision.allowed {
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_str(&reset_seconds.to_string())
                .unwrap_or_else(|_| HeaderValue::from_static("1")),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

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

        assert!(limiter.check("first", now).allowed);
        assert!(limiter.check("first", now).allowed);
        assert!(!limiter.check("first", now).allowed);
        assert!(limiter.check("second", now).allowed);
        assert!(limiter.check("first", now + Duration::from_mins(1)).allowed);
    }
}
