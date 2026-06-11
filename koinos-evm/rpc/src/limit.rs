//! Per-client-IP token-bucket rate limiting.
//!
//! Hand-rolled (no middleware dep): one bucket per client IP, refilled at
//! `rps` tokens/second up to `burst`. A JSON-RPC batch costs one token per
//! contained request, so batching can't bypass the limit.

use axum::http::HeaderMap;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

/// Resolve the client IP to rate-limit on. Direct connections always use the
/// TCP peer address. Only when the peer is LOOPBACK and `trust` is enabled
/// (TRUST_PROXY_HEADERS=1, i.e. a reverse proxy on this same host fronts us —
/// without this every visitor behind nginx shares one bucket) do proxy-set
/// headers win: X-Real-IP first (single value our nginx sets), else the LAST
/// X-Forwarded-For hop — the one our proxy appended; earlier entries are
/// client-forgeable and must not be trusted.
pub fn effective_client_ip(peer: IpAddr, headers: &HeaderMap, trust: bool) -> IpAddr {
    if !trust || !peer.is_loopback() {
        return peer;
    }
    if let Some(ip) = headers
        .get("x-real-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok())
    {
        return ip;
    }
    if let Some(ip) = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .next_back()
        })
        .and_then(|s| s.parse().ok())
    {
        return ip;
    }
    peer
}

/// Bound on the bucket map itself — it is keyed by attacker-controlled IPs.
/// On overflow, buckets idle for 60+ seconds are swept (an idle bucket has
/// refilled to full anyway, so dropping it loses nothing).
const MAX_BUCKETS: usize = 10_000;
const IDLE_SWEEP_SECS: u64 = 60;

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

pub struct RateLimiter {
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
    /// Sustained refill rate (tokens/second). 0 = limiter disabled.
    rps: f64,
    /// Bucket capacity (burst size).
    burst: f64,
}

impl RateLimiter {
    pub fn new(rps: u64, burst: u64) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            rps: rps as f64,
            // A zero/below-rps burst would make the limiter reject everything.
            burst: burst.max(rps.max(1)) as f64,
        }
    }

    pub fn enabled(&self) -> bool {
        self.rps > 0.0
    }

    /// Try to take `cost` tokens for `ip`. Returns false when the client is
    /// over its limit. cost is clamped to >= 1.
    pub fn allow(&self, ip: IpAddr, cost: usize) -> bool {
        if !self.enabled() {
            return true;
        }
        let cost = (cost.max(1)) as f64;
        let now = Instant::now();
        let mut buckets = self.buckets.lock().unwrap();

        if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(&ip) {
            buckets.retain(|_, b| now.duration_since(b.last_refill).as_secs() < IDLE_SWEEP_SECS);
            if buckets.len() >= MAX_BUCKETS {
                // Map still full of *active* clients — refuse new ones rather
                // than growing without bound.
                return false;
            }
        }

        let bucket = buckets.entry(ip).or_insert(Bucket {
            tokens: self.burst,
            last_refill: now,
        });
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * self.rps).min(self.burst);
        bucket.last_refill = now;
        if bucket.tokens >= cost {
            bucket.tokens -= cost;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn proxy_headers_ignored_without_trust() {
        let peer = ip(9);
        let h = hm(&[("x-real-ip", "1.2.3.4")]);
        assert_eq!(effective_client_ip(peer, &h, false), peer);
    }

    #[test]
    fn proxy_headers_ignored_for_nonloopback_peer() {
        // A direct (non-proxied) client cannot pick its own bucket by forging headers.
        let peer = ip(9);
        let h = hm(&[("x-real-ip", "1.2.3.4"), ("x-forwarded-for", "5.6.7.8")]);
        assert_eq!(effective_client_ip(peer, &h, true), peer);
    }

    #[test]
    fn real_ip_wins_from_loopback() {
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let h = hm(&[("x-real-ip", "1.2.3.4")]);
        assert_eq!(
            effective_client_ip(peer, &h, true),
            "1.2.3.4".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn xff_last_hop_used_not_first() {
        // First entry is client-supplied garbage; the LAST was appended by our nginx.
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let h = hm(&[("x-forwarded-for", "6.6.6.6, 203.0.113.7")]);
        assert_eq!(
            effective_client_ip(peer, &h, true),
            "203.0.113.7".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn malformed_headers_fall_back_to_peer() {
        let peer: IpAddr = "127.0.0.1".parse().unwrap();
        let h = hm(&[("x-real-ip", "not-an-ip"), ("x-forwarded-for", "also-bad")]);
        assert_eq!(effective_client_ip(peer, &h, true), peer);
    }

    #[test]
    fn burst_then_reject() {
        let rl = RateLimiter::new(1, 3);
        assert!(rl.allow(ip(1), 1));
        assert!(rl.allow(ip(1), 1));
        assert!(rl.allow(ip(1), 1));
        // Bucket drained; sustained rate is 1/s so an immediate 4th call fails.
        assert!(!rl.allow(ip(1), 1));
        // Other clients are unaffected.
        assert!(rl.allow(ip(2), 1));
    }

    #[test]
    fn batch_cost_counts_per_request() {
        let rl = RateLimiter::new(1, 10);
        assert!(rl.allow(ip(1), 10)); // a 10-item batch drains the bucket
        assert!(!rl.allow(ip(1), 1));
    }

    #[test]
    fn disabled_when_rps_zero() {
        let rl = RateLimiter::new(0, 0);
        for _ in 0..1000 {
            assert!(rl.allow(ip(1), 100));
        }
    }
}
