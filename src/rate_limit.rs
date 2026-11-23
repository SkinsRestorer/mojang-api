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
    metrics::Metrics,
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
/// The number of clients a full shard inspects to choose one to evict.
const EVICTION_SAMPLE_SIZE: usize = 8;

/// The key that identifies a client, which the rate limit middleware adds to each request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientKey(pub IpAddr);

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
    metrics: Arc<Metrics>,
}

#[derive(Debug)]
struct RateLimiterState {
    clients: Clients,
    last_cleanup: Instant,
}

/// The theoretical arrival time of each client's next request.
///
/// Entries are stored densely, so a full shard can sample them by position in constant time.
#[derive(Debug, Default)]
struct Clients {
    positions: HashMap<IpAddr, usize>,
    entries: Vec<(IpAddr, Instant)>,
}

impl Clients {
    fn len(&self) -> usize {
        self.entries.len()
    }

    fn get(&self, key: IpAddr) -> Option<Instant> {
        self.positions
            .get(&key)
            .map(|&position| self.entries[position].1)
    }

    fn insert(&mut self, key: IpAddr, arrival: Instant) {
        if let Some(&position) = self.positions.get(&key) {
            self.entries[position].1 = arrival;
        } else {
            self.positions.insert(key, self.entries.len());
            self.entries.push((key, arrival));
        }
    }

    fn remove_at(&mut self, position: usize) {
        let (key, _) = self.entries.swap_remove(position);
        self.positions.remove(&key);
        if let Some(&(moved, _)) = self.entries.get(position) {
            self.positions.insert(moved, position);
        }
    }

    /// Removes clients whose quota is full again at `now`.
    fn remove_expired(&mut self, now: Instant) {
        self.entries.retain(|&(_, arrival)| arrival > now);
        self.positions.clear();
        self.positions.extend(
            self.entries
                .iter()
                .enumerate()
                .map(|(position, &(key, _))| (key, position)),
        );
    }

    /// Evicts the client closest to a full quota among a few clients from `start` onward.
    ///
    /// Sampling keeps eviction constant time. Clients with a backlog survive unless every
    /// sampled client has a larger one.
    fn evict_sampled(&mut self, start: usize) {
        let len = self.len();
        if len == 0 {
            return;
        }
        let victim = (0..EVICTION_SAMPLE_SIZE.min(len))
            .map(|offset| (start + offset) % len)
            .min_by_key(|&position| self.entries[position].1);
        if let Some(position) = victim {
            self.remove_at(position);
        }
    }
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
    /// Requests from other peers are limited by the peer address. Rejected requests are
    /// counted in `metrics`.
    ///
    /// # Panics
    ///
    /// Never in practice: the policy header contains only digits, `;`, and `w=`.
    #[must_use]
    pub fn new(
        limit: NonZeroU32,
        window: Duration,
        trusted_proxies: Vec<IpNet>,
        metrics: Arc<Metrics>,
    ) -> Self {
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
                        clients: Clients::default(),
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
            metrics,
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
        let hash = self.shard_hasher.hash_one(key);
        // Reduces the 64-bit hash before narrowing it, so 32-bit targets use every shard.
        let shard_index = usize::try_from(hash % RATE_LIMIT_SHARDS as u64).unwrap_or_default();
        let mut state = self.shards[shard_index]
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let is_new = state.clients.get(key).is_none();
        let since_cleanup = now.saturating_duration_since(state.last_cleanup);
        let shard_full = is_new && state.clients.len() >= MAX_CLIENTS_PER_SHARD;
        if since_cleanup >= self.window
            || (shard_full && since_cleanup >= MIN_FULL_CLEANUP_INTERVAL)
        {
            state.clients.remove_expired(now);
            state.last_cleanup = now;
        }
        if is_new && state.clients.len() >= MAX_CLIENTS_PER_SHARD {
            // The hash bits above the shard index are unpredictable to clients, so they pick
            // a random sample without letting a client steer which entries are inspected.
            let start =
                usize::try_from(hash / RATE_LIMIT_SHARDS as u64 % MAX_CLIENTS_PER_SHARD as u64)
                    .unwrap_or_default();
            state.clients.evict_sampled(start);
        }

        let arrival = state
            .clients
            .get(key)
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
    mut request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip());
    let key = rate_limiter.client_key(peer, request.headers());
    let decision = rate_limiter.check(key, Instant::now());

    let mut response = if decision.allowed {
        request.extensions_mut().insert(ClientKey(key));
        next.run(request).await
    } else {
        rate_limiter.metrics.increment_rate_limited_requests();
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
        sync::Arc,
        time::{Duration, Instant},
    };

    use axum::http::{HeaderMap, HeaderValue};

    use std::hash::BuildHasher;

    use super::{
        CF_CONNECTING_IP, Clients, EVICTION_SAMPLE_SIZE, MAX_CLIENTS_PER_SHARD, RATE_LIMIT_SHARDS,
        RateLimiter,
    };

    fn limiter(limit: u32, window: Duration) -> RateLimiter {
        RateLimiter::new(
            NonZeroU32::new(limit).expect("test limit should not be zero"),
            window,
            vec!["10.0.0.0/8".parse().expect("test network should parse")],
            Arc::default(),
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

    /// Returns how many clients the shard that holds `key` tracks.
    fn clients_in_shard_of(limiter: &RateLimiter, key: IpAddr) -> usize {
        limiter
            .shards
            .iter()
            .map(|shard| {
                shard
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
            })
            .find(|state| state.clients.get(key).is_some())
            .map(|state| state.clients.len())
            .expect("a checked client should be tracked")
    }

    #[test]
    fn bounds_memory_per_shard() {
        // A full quota refills after 60 ms, so every client below is idle again after that.
        let limiter = limiter(1_000, Duration::from_mins(1));
        let start = Instant::now();
        let clients = u32::try_from(MAX_CLIENTS_PER_SHARD * RATE_LIMIT_SHARDS * 2)
            .expect("test size should fit");
        for index in 0..clients {
            limiter.check(IpAddr::V4(Ipv4Addr::from_bits(index)), start);
        }
        for shard in &limiter.shards {
            let clients = shard
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clients
                .len();
            assert_eq!(clients, MAX_CLIENTS_PER_SHARD, "every shard should fill up");
        }

        // Scanning a full shard is slow, so idle clients are only cleared once per second.
        let early = IpAddr::V4(Ipv4Addr::from_bits(clients));
        limiter.check(early, start + Duration::from_millis(500));
        assert_eq!(clients_in_shard_of(&limiter, early), MAX_CLIENTS_PER_SHARD);

        let late = IpAddr::V4(Ipv4Addr::from_bits(clients + 1));
        limiter.check(late, start + Duration::from_millis(1_500));
        assert_eq!(
            clients_in_shard_of(&limiter, late),
            1,
            "a full shard should drop idle clients before it evicts active ones"
        );
    }

    #[test]
    fn forgets_idle_clients_once_per_window() {
        let limiter = limiter(1, Duration::from_mins(1));
        let start = Instant::now();
        for index in 0..1_000 {
            limiter.check(IpAddr::V4(Ipv4Addr::from_bits(index)), start);
        }
        let tracked: usize = limiter
            .shards
            .iter()
            .map(|shard| {
                shard
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clients
                    .len()
            })
            .sum();
        assert_eq!(tracked, 1_000, "shards below the cap keep every client");

        let returning = IpAddr::V4(Ipv4Addr::from_bits(0));
        assert!(
            !limiter
                .check(returning, start + Duration::from_secs(30))
                .allowed
        );
        let next_window = start + Duration::from_secs(61);
        assert!(limiter.check(returning, next_window).allowed);
        assert_eq!(clients_in_shard_of(&limiter, returning), 1);
    }

    /// Returns `count` distinct IPv4 clients that share a shard with `target`.
    fn same_shard_clients(limiter: &RateLimiter, target: IpAddr, count: usize) -> Vec<IpAddr> {
        let shard = limiter.shard_hasher.hash_one(target) % RATE_LIMIT_SHARDS as u64;
        (1..u32::MAX)
            .map(|index| IpAddr::V4(Ipv4Addr::from_bits(index)))
            .filter(|&client| {
                client != target
                    && limiter.shard_hasher.hash_one(client) % RATE_LIMIT_SHARDS as u64 == shard
            })
            .take(count)
            .collect()
    }

    #[test]
    fn eviction_keeps_the_backlog_of_a_rate_limited_client() {
        let limiter = limiter(2, Duration::from_mins(1));
        let now = Instant::now();
        let backlogged = address("203.0.113.9");
        assert!(
            limiter.check(backlogged, now).allowed,
            "first request is allowed"
        );
        assert!(limiter.check(backlogged, now).allowed, "burst is allowed");
        assert!(
            !limiter.check(backlogged, now).allowed,
            "client is rate limited"
        );

        // Fills the shard, then makes each original client be replaced twice over.
        for client in same_shard_clients(&limiter, backlogged, MAX_CLIENTS_PER_SHARD * 3) {
            assert!(limiter.check(client, now).allowed, "new client is allowed");
        }

        let decision = limiter.check(backlogged, now);
        assert!(
            !decision.allowed,
            "the rate-limited client keeps its backlog"
        );
        assert_eq!(decision.retry_after, Duration::from_secs(30));
    }

    #[test]
    fn evicts_the_sampled_client_closest_to_a_full_quota() {
        let now = Instant::now();
        let mut clients = Clients::default();
        let keys: Vec<IpAddr> = (0..u32::try_from(EVICTION_SAMPLE_SIZE * 2).expect("fits"))
            .map(|index| IpAddr::V4(Ipv4Addr::from_bits(index)))
            .collect();
        for (offset, &key) in (1..).zip(&keys) {
            clients.insert(key, now + Duration::from_secs(offset));
        }
        // Position 3 has the smallest arrival time in a sample that starts there.
        clients.evict_sampled(3);
        assert_eq!(clients.len(), keys.len() - 1);
        assert_eq!(clients.get(keys[3]), None, "the closest client is evicted");
        for (offset, &key) in (1..).zip(&keys) {
            if key != keys[3] {
                assert_eq!(
                    clients.get(key),
                    Some(now + Duration::from_secs(offset)),
                    "other clients keep their state after the swap"
                );
            }
        }

        // A sample that wraps past the end still finds the smallest arrival time.
        clients.evict_sampled(clients.len() - 2);
        assert_eq!(
            clients.get(keys[0]),
            None,
            "the wrapped sample reaches the front"
        );
    }

    #[test]
    fn removing_expired_clients_keeps_the_rest_reachable() {
        let now = Instant::now();
        let mut clients = Clients::default();
        let expired = address("192.0.2.1");
        let active = address("192.0.2.2");
        clients.insert(expired, now);
        clients.insert(active, now + Duration::from_secs(1));
        clients.remove_expired(now);
        assert_eq!(clients.len(), 1);
        assert_eq!(clients.get(expired), None);
        assert_eq!(clients.get(active), Some(now + Duration::from_secs(1)));
        clients.insert(active, now + Duration::from_secs(2));
        assert_eq!(clients.len(), 1, "updating a client does not add an entry");
    }
}
