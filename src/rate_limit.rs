use std::{
    collections::HashMap,
    hash::{DefaultHasher, Hash, Hasher},
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, StatusCode,
        header::{RETRY_AFTER, X_CONTENT_TYPE_OPTIONS},
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use ipnet::IpNet;
use thiserror::Error;

use crate::{
    error::ceil_seconds,
    types::{ErrorResponse, ErrorType},
};

const CF_CONNECTING_IP: HeaderName = HeaderName::from_static("cf-connecting-ip");
const RATE_LIMIT_LIMIT: HeaderName = HeaderName::from_static("ratelimit-limit");
const RATE_LIMIT_POLICY: HeaderName = HeaderName::from_static("ratelimit-policy");
const RATE_LIMIT_REMAINING: HeaderName = HeaderName::from_static("ratelimit-remaining");
const RATE_LIMIT_RESET: HeaderName = HeaderName::from_static("ratelimit-reset");
const RATE_LIMIT_SHARDS: usize = 32;
/// Bounds limiter memory to roughly `RATE_LIMIT_SHARDS * MAX_CLIENTS_PER_SHARD` entries.
const MAX_CLIENTS_PER_SHARD: usize = 4_096;
/// Limits how often a full shard is scanned for expired clients.
const MIN_FULL_CLEANUP_INTERVAL: Duration = Duration::from_secs(1);

/// A generic cell rate algorithm (GCRA) limiter.
///
/// Each client may send `limit` requests in a burst, then one request per `window / limit`.
/// Unlike a fixed window, a client cannot send twice the limit across a window boundary.
#[derive(Debug)]
pub struct RateLimiter {
    limit: u32,
    window: Duration,
    emission_interval: Duration,
    burst_tolerance: Duration,
    trusted_proxies: Vec<IpNet>,
    shards: Box<[Mutex<RateLimiterState>]>,
    limit_header: HeaderValue,
    policy_header: HeaderValue,
}

#[derive(Debug)]
struct RateLimiterState {
    /// The theoretical arrival time of each client's next request.
    clients: HashMap<IpAddr, Instant>,
    last_cleanup: Instant,
}

#[derive(Debug, Clone, Copy)]
struct Decision {
    allowed: bool,
    remaining: u32,
    reset_after: Duration,
    retry_after: Duration,
}

impl RateLimiter {
    /// Creates an in-memory rate limiter.
    ///
    /// `trusted_proxies` lists the peers whose `CF-Connecting-IP` header identifies the client.
    /// Requests from other peers are limited by the peer address.
    ///
    /// # Errors
    ///
    /// Returns an error when the limit or window is zero.
    pub fn new(
        limit: u32,
        window: Duration,
        trusted_proxies: Vec<IpNet>,
    ) -> Result<Self, RateLimitConfigError> {
        if limit == 0 {
            return Err(RateLimitConfigError::ZeroLimit);
        }
        if window.is_zero() {
            return Err(RateLimitConfigError::ZeroWindow);
        }
        let policy_header = HeaderValue::from_str(&format!("{limit};w={}", window.as_secs()))
            .map_err(|error| {
                tracing::error!(%error, "could not construct rate limit policy header");
                RateLimitConfigError::InvalidPolicyHeader
            })?;
        let emission_interval = (window / limit).max(Duration::from_nanos(1));
        let now = Instant::now();
        Ok(Self {
            limit,
            window,
            emission_interval,
            burst_tolerance: window.saturating_sub(emission_interval),
            trusted_proxies,
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

    fn client_key(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> IpAddr {
        let Some(peer) = peer.map(|peer| peer.to_canonical()) else {
            return IpAddr::V6(Ipv6Addr::UNSPECIFIED);
        };
        let client = if self
            .trusted_proxies
            .iter()
            .any(|network| network.contains(&peer))
        {
            headers
                .get(&CF_CONNECTING_IP)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.trim().parse::<IpAddr>().ok())
                .map_or(peer, |address| address.to_canonical())
        } else {
            peer
        };
        match client {
            // One IPv6 subscriber usually controls a whole /64.
            IpAddr::V6(address) => IpAddr::V6(Ipv6Addr::from_bits(
                address.to_bits() & !u128::from(u64::MAX),
            )),
            IpAddr::V4(_) => client,
        }
    }

    fn check(&self, key: IpAddr, now: Instant) -> Decision {
        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let shard_index =
            usize::try_from(hasher.finish()).unwrap_or(usize::MAX) % self.shards.len();
        let mut state = self.shards[shard_index]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let is_new = !state.clients.contains_key(&key);
        let since_cleanup = now.saturating_duration_since(state.last_cleanup);
        let shard_full = is_new && state.clients.len() >= MAX_CLIENTS_PER_SHARD;
        if since_cleanup >= self.window
            || (shard_full && since_cleanup >= MIN_FULL_CLEANUP_INTERVAL)
        {
            state.clients.retain(|_, arrival| *arrival > now);
            state.last_cleanup = now;
        }
        if is_new
            && state.clients.len() >= MAX_CLIENTS_PER_SHARD
            && let Some(evicted) = state.clients.keys().next().copied()
        {
            // Evicting an arbitrary client keeps the shard bounded in constant time.
            state.clients.remove(&evicted);
        }

        let arrival = state
            .clients
            .get(&key)
            .copied()
            .map_or(now, |arrival| arrival.max(now));
        let backlog = arrival.saturating_duration_since(now);
        if backlog > self.burst_tolerance {
            drop(state);
            return Decision {
                allowed: false,
                remaining: 0,
                reset_after: backlog,
                retry_after: backlog.saturating_sub(self.burst_tolerance),
            };
        }

        let next_arrival = arrival
            .checked_add(self.emission_interval)
            .unwrap_or(arrival);
        state.clients.insert(key, next_arrival);
        drop(state);

        let backlog = next_arrival.saturating_duration_since(now);
        let remaining =
            self.window.saturating_sub(backlog).as_nanos() / self.emission_interval.as_nanos();
        Decision {
            allowed: true,
            remaining: u32::try_from(remaining).unwrap_or(u32::MAX).min(self.limit),
            reset_after: backlog,
            retry_after: Duration::ZERO,
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
    State(rate_limiter): State<Arc<RateLimiter>>,
    request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip());
    let key = rate_limiter.client_key(peer, request.headers());
    let decision = rate_limiter.check(key, Instant::now());

    let mut response = if decision.allowed {
        next.run(request).await
    } else {
        (
            StatusCode::TOO_MANY_REQUESTS,
            Json(ErrorResponse {
                error: ErrorType::RateLimited,
            }),
        )
            .into_response()
    };
    let headers = response.headers_mut();
    headers.insert(RATE_LIMIT_LIMIT, rate_limiter.limit_header.clone());
    headers.insert(RATE_LIMIT_POLICY, rate_limiter.policy_header.clone());
    headers.insert(RATE_LIMIT_REMAINING, HeaderValue::from(decision.remaining));
    headers.insert(
        RATE_LIMIT_RESET,
        HeaderValue::from(ceil_seconds(decision.reset_after)),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if !decision.allowed {
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from(ceil_seconds(decision.retry_after)),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        time::{Duration, Instant},
    };

    use axum::http::{HeaderMap, HeaderValue};

    use super::{CF_CONNECTING_IP, MAX_CLIENTS_PER_SHARD, RateLimitConfigError, RateLimiter};

    fn limiter(limit: u32, window: Duration) -> RateLimiter {
        RateLimiter::new(
            limit,
            window,
            vec!["10.0.0.0/8".parse().expect("test network should parse")],
        )
        .expect("test rate limiter configuration should be valid")
    }

    fn address(value: &str) -> IpAddr {
        value.parse().expect("test address should parse")
    }

    fn forwarded_for(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            CF_CONNECTING_IP,
            HeaderValue::from_str(value).expect("test header should be valid"),
        );
        headers
    }

    #[test]
    fn rejects_zero_rate_limit_configuration_values() {
        assert!(matches!(
            RateLimiter::new(0, Duration::from_secs(1), Vec::new()),
            Err(RateLimitConfigError::ZeroLimit)
        ));
        assert!(matches!(
            RateLimiter::new(1, Duration::ZERO, Vec::new()),
            Err(RateLimitConfigError::ZeroWindow)
        ));
    }

    #[test]
    fn limits_each_client_independently() {
        let limiter = limiter(2, Duration::from_mins(1));
        let now = Instant::now();
        let first = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));
        let second = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 2));

        let decision = limiter.check(first, now);
        assert!(decision.allowed, "first request is allowed");
        assert_eq!(decision.remaining, 1);
        assert!(limiter.check(first, now).allowed, "burst is allowed");
        let rejected = limiter.check(first, now);
        assert!(!rejected.allowed, "third request exceeds the limit");
        assert_eq!(rejected.retry_after, Duration::from_secs(30));
        assert!(
            limiter.check(second, now).allowed,
            "other client is allowed"
        );
        assert!(
            limiter.check(first, now + Duration::from_mins(1)).allowed,
            "quota refills after the window"
        );
    }

    #[test]
    fn does_not_allow_double_bursts_across_a_window_boundary() {
        let limiter = limiter(10, Duration::from_mins(1));
        let client = address("192.0.2.1");
        let start = Instant::now();
        assert!(
            limiter.check(client, start).allowed,
            "first request is allowed"
        );

        // A fixed window that opened at `start` would allow 10 requests just before it closes
        // and another 10 just after it reopens.
        let before_boundary = start + Duration::from_secs(59);
        for _ in 0..10 {
            assert!(
                limiter.check(client, before_boundary).allowed,
                "burst is allowed"
            );
        }
        assert!(
            !limiter
                .check(client, before_boundary + Duration::from_secs(2))
                .allowed,
            "no second burst after the boundary"
        );

        let refill = before_boundary + Duration::from_secs(6);
        assert!(limiter.check(client, refill).allowed, "one request refills");
        assert!(!limiter.check(client, refill).allowed, "only one refills");
    }

    #[test]
    fn trusts_the_forwarded_address_only_from_trusted_proxies() {
        let limiter = limiter(1, Duration::from_mins(1));
        let headers = forwarded_for("198.51.100.7");

        assert_eq!(
            limiter.client_key(Some(address("10.1.1.1")), &headers),
            address("198.51.100.7")
        );
        assert_eq!(
            limiter.client_key(Some(address("203.0.113.5")), &headers),
            address("203.0.113.5"),
            "an untrusted peer cannot choose its rate limit key"
        );
        assert_eq!(
            limiter.client_key(Some(address("10.1.1.1")), &HeaderMap::new()),
            address("10.1.1.1"),
            "a trusted peer without the header is limited by its own address"
        );
    }

    #[test]
    fn groups_ipv6_clients_by_64_bit_prefix() {
        let limiter = limiter(1, Duration::from_mins(1));
        let first = limiter.client_key(
            Some(address("10.0.0.1")),
            &forwarded_for("2001:db8:1:2:aaaa::1"),
        );
        let second = limiter.client_key(Some(address("2001:db8:1:2:bbbb::9")), &HeaderMap::new());
        assert_eq!(first, address("2001:db8:1:2::"));
        assert_eq!(first, second);
    }

    #[test]
    fn bounds_memory_per_shard() {
        let limiter = limiter(1, Duration::from_hours(1));
        let now = Instant::now();
        for index in 0..u32::try_from(MAX_CLIENTS_PER_SHARD * super::RATE_LIMIT_SHARDS * 2)
            .expect("test size should fit")
        {
            limiter.check(IpAddr::V4(Ipv4Addr::from_bits(index)), now);
        }
        for shard in &limiter.shards {
            let clients = shard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clients
                .len();
            assert!(clients <= MAX_CLIENTS_PER_SHARD, "shard holds {clients}");
        }
    }
}
