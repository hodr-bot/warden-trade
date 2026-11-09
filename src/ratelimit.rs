//! Per-IP sliding-window rate limiter for the auth endpoints.
//!
//! No dependency by design: a mutex-guarded map of recent attempt
//! timestamps per client IP, pruned on every check. Budgets are deliberately
//! tight on `/api/auth/*` because each attempt burns an Argon2 hash —
//! unauthenticated CPU spend is the spam vector (see the signup audit).
//! Limits are process env (`AUTH_RATE_MAX`, `AUTH_RATE_WINDOW_SECS`), not a
//! DB flag, so agents cannot loosen their own gate.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Sliding-window limiter: at most `max_hits` allowed hits per `window`.
pub struct RateLimiter {
    hits: Mutex<HashMap<IpAddr, Vec<Instant>>>,
    max_hits: u32,
    window: Duration,
}

impl RateLimiter {
    pub fn new(max_hits: u32, window_secs: u64) -> Self {
        Self {
            hits: Mutex::new(HashMap::new()),
            max_hits,
            window: Duration::from_secs(window_secs.max(1)),
        }
    }

    /// Budget from env, defaulting to 10 attempts per 60 s per IP.
    pub fn from_env() -> Self {
        let max = std::env::var("AUTH_RATE_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let window = std::env::var("AUTH_RATE_WINDOW_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        Self::new(max, window)
    }

    /// Returns `true` when the attempt is allowed (and records it),
    /// `false` when the IP is over budget. Prunes expired entries first so
    /// idle IPs never grow the map.
    pub fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap();
        let entry = hits.entry(ip).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        if entry.len() as u32 >= self.max_hits {
            return false;
        }
        entry.push(now);
        if hits.len() > 10_000 {
            hits.retain(|_, v| v.iter().any(|t| now.duration_since(*t) < self.window));
        }
        true
    }

    /// Seconds until the oldest hit in the window expires (for `Retry-After`).
    pub fn retry_after_secs(&self, ip: IpAddr) -> u64 {
        let now = Instant::now();
        let hits = self.hits.lock().unwrap();
        hits.get(&ip)
            .and_then(|v| v.iter().min())
            .map(|t| {
                let elapsed = now.duration_since(*t).as_secs();
                self.window.as_secs().saturating_sub(elapsed).max(1)
            })
            .unwrap_or(1)
    }
}

/// Best-effort client IP behind Cloudflare: `CF-Connecting-IP` wins, then
/// the first `X-Forwarded-For` entry, else the TCP peer address (always
/// 127.0.0.1 behind cloudflared, hence the header preference).
pub fn client_ip(headers: &axum::http::HeaderMap, peer: Option<IpAddr>) -> Option<IpAddr> {
    if let Some(v) = headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok()) {
        if let Ok(ip) = v.trim().parse() {
            return Some(ip);
        }
    }
    if let Some(v) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = v.split(',').next() {
            if let Ok(ip) = first.trim().parse() {
                return Some(ip);
            }
        }
    }
    peer
}

/// Axum middleware for the auth sub-router: denies over-budget IPs with
/// `429 + Retry-After` before any Argon2 work happens.
pub async fn limit_auth(
    axum::extract::State(limiter): axum::extract::State<std::sync::Arc<RateLimiter>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Behind cloudflared the TCP peer is always loopback, so identity comes
    // from the proxy headers first; direct hits fall back to the TCP peer
    // (read from extensions: into_make_service_with_connect_info inserts it).
    // Never a global bucket: one LAN client must not eat everyone's budget.
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.ip());
    let ip = client_ip(req.headers(), peer);
    match ip {
        Some(ip) if limiter.allow(ip) => next.run(req).await,
        Some(ip) => (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            [(
                axum::http::header::RETRY_AFTER,
                axum::http::HeaderValue::from(limiter.retry_after_secs(ip)),
            )],
            axum::Json(serde_json::json!({ "error": "too many attempts, slow down" })),
        )
            .into_response(),
        // No identifiable IP (should not happen behind Cloudflare): fail
        // closed rather than giving the endpoint unlimited budget.
        None => (
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            axum::Json(serde_json::json!({ "error": "too many attempts, slow down" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip() -> IpAddr {
        "203.0.113.7".parse().unwrap()
    }

    #[test]
    fn burst_over_budget_denied_then_refills() {
        let lim = RateLimiter::new(3, 60);
        assert!(lim.allow(ip()));
        assert!(lim.allow(ip()));
        assert!(lim.allow(ip()));
        assert!(!lim.allow(ip()), "4th hit in the window must be denied");
        assert!(!lim.allow(ip()), "still denied while window is full");
    }

    #[test]
    fn window_expiry_refills_budget() {
        let lim = RateLimiter::new(1, 1);
        assert!(lim.allow(ip()));
        assert!(!lim.allow(ip()));
        std::thread::sleep(Duration::from_millis(1100));
        assert!(lim.allow(ip()), "budget must refill after the window");
    }

    #[test]
    fn budgets_are_per_ip() {
        let lim = RateLimiter::new(1, 60);
        assert!(lim.allow(ip()));
        assert!(lim.allow("198.51.100.9".parse().unwrap()), "other IPs unaffected");
    }

    #[test]
    fn client_ip_prefers_cloudflare_header() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("cf-connecting-ip", "203.0.113.7".parse().unwrap());
        h.insert("x-forwarded-for", "198.51.100.9".parse().unwrap());
        assert_eq!(client_ip(&h, None), Some(ip()));
    }
}
