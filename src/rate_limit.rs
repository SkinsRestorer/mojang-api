use std::{
    collections::HashMap,
    hash::{BuildHasher, RandomState},
    net::{IpAddr, Ipv6Addr, SocketAddr},
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Json,
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header::RETRY_AFTER},
    middleware::Next,
    response::{IntoResponse, Response},
};
use ipnet::IpNet;

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
    window: Duration,
    emission_interval: Duration,
    burst_tolerance: Duration,
    trusted_proxies: Vec<IpNet>,
    /// Seeds shard selection randomly, so clients cannot choose addresses that share a shard.
    shard_hasher: RandomState,
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

struct Decision {
    allowed: bool,
    remaining: u32,
    reset_after: Duration,
    retry_after: Duration,
}

impl RateLimiter {
    /// Creates an in-memory rate limiter that allows `limit` requests per `window`.
    ///
    /// `trusted_proxies` lists the peers whose `CF-Connecting-IP` header identifies the client.
    /// Requests from other peers are limited by the peer address.
    ///
    /// # Panics
    ///
    /// Never in practice: the policy header contains only digits, `;`, and `w=`.
    #[must_use]
    pub fn new(limit: NonZeroU32, window: Duration, trusted_proxies: Vec<IpNet>) -> Self {
        let emission_interval = (window / limit.get()).max(Duration::from_nanos(1));
        let now = Instant::now();
        Self {
            window,
            emission_interval,
            // Derived from the rounded interval, so a window that is not a whole number of
            // intervals cannot admit an extra request.
            burst_tolerance: emission_interval.saturating_mul(limit.get() - 1),
            trusted_proxies,
            shard_hasher: RandomState::new(),
            shards: (0..RATE_LIMIT_SHARDS)
                .map(|_| {
                    Mutex::new(RateLimiterState {
                        clients: HashMap::new(),
                        last_cleanup: now,
                    })
                })
                .collect(),
            limit_header: HeaderValue::from(limit.get()),
            #[expect(
                clippy::expect_used,
                reason = "Digits, a semicolon, and `w=` are valid header characters."
            )]
            policy_header: HeaderValue::try_from(format!("{limit};w={}", window.as_secs()))
                .expect("the rate limit policy is a valid header value"),
        }
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
        // Reduces the 64-bit hash before narrowing it, so 32-bit targets use every shard.
        let shard_index =
            usize::try_from(self.shard_hasher.hash_one(key) % RATE_LIMIT_SHARDS as u64)
                .unwrap_or_default();
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
        // The next request is allowed while its backlog stays within the burst tolerance.
        let remaining = (self.burst_tolerance + self.emission_interval)
            .saturating_sub(backlog)
            .as_nanos()
            / self.emission_interval.as_nanos();
        Decision {
            allowed: true,
            remaining: u32::try_from(remaining).unwrap_or(u32::MAX),
            reset_after: backlog,
            retry_after: Duration::ZERO,
        }
    }
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
        num::NonZeroU32,
        time::{Duration, Instant},
    };

    use axum::http::{HeaderMap, HeaderValue};

    use super::{CF_CONNECTING_IP, MAX_CLIENTS_PER_SHARD, RateLimiter};

    fn limiter(limit: u32, window: Duration) -> RateLimiter {
        RateLimiter::new(
            NonZeroU32::new(limit).expect("test limit should not be zero"),
            window,
            vec!["10.0.0.0/8".parse().expect("test network should parse")],
        )
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
    fn allows_exactly_the_limit_when_the_window_is_not_a_whole_number_of_intervals() {
        // 11 ns / 4 rounds the interval down to 2 ns.
        let limiter = limiter(4, Duration::from_nanos(11));
        let client = address("192.0.2.1");
        let now = Instant::now();
        let remaining: Vec<u32> = (0..4)
            .map(|_| {
                let decision = limiter.check(client, now);
                assert!(decision.allowed, "requests within the limit are allowed");
                decision.remaining
            })
            .collect();
        assert_eq!(remaining, [3, 2, 1, 0]);
        assert!(
            !limiter.check(client, now).allowed,
            "the fifth request is rejected"
        );
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
        // A dual-stack listener reports IPv4 clients as IPv4-mapped IPv6 addresses, which
        // must not share one IPv6 /64 key.
        assert_eq!(
            limiter.client_key(Some(address("::ffff:203.0.113.5")), &HeaderMap::new()),
            address("203.0.113.5")
        );
        assert_eq!(
            limiter.client_key(Some(address("::ffff:10.1.1.1")), &headers),
            address("198.51.100.7"),
            "a mapped proxy address is still trusted"
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
