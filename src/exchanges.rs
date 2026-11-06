//! Exchange abstractions for paper-trading market feeds.
//!
//! Phase 2: shared error taxonomy + [`MarketTick`] + [`ExchangeClient`] trait.
//! No live keys, no trading, no network calls here — concrete clients
//! (Coinbase, Kraken, …) implement [`ExchangeClient`] elsewhere.
//!
//! Multi-tenancy note: ticks are public market data (no `tenant_id`);
//! any persisted rows derived from ticks must carry `tenant_id`.

use std::fmt;

/// Shared error taxonomy for all exchange clients.
///
/// Each variant carries a human-readable detail string; `RateLimited`
/// additionally carries an optional `retry_after_secs` hint when the
/// exchange advertises one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExchangeError {
    /// Request timed out (connect or read). Retry may be safe for reads.
    Timeout(String),
    /// Transport failure: DNS, connection refused/reset, TLS, ….
    Network(String),
    /// HTTP 429 or exchange-specific rate-limit signal.
    RateLimited {
        detail: String,
        retry_after_secs: Option<u64>,
    },
    /// HTTP 4xx/5xx (non-auth, non-429, non-404) or undecodable payload.
    BadResponse(String),
    /// HTTP 401/403, bad key, bad signature, insufficient scope.
    Auth(String),
    /// HTTP 404 or unknown symbol/market.
    NotFound(String),
    /// Outcome unknown (e.g. timeout after send): retrying may duplicate.
    /// Callers must reconcile state before acting.
    Uncertain(String),
}

impl fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout(d) => write!(f, "exchange timeout: {d}"),
            Self::Network(d) => write!(f, "exchange network error: {d}"),
            Self::RateLimited {
                detail,
                retry_after_secs: Some(secs),
            } => {
                write!(f, "exchange rate limited: {detail} (retry after {secs}s)")
            }
            Self::RateLimited {
                detail,
                retry_after_secs: None,
            } => write!(f, "exchange rate limited: {detail}"),
            Self::BadResponse(d) => write!(f, "exchange bad response: {d}"),
            Self::Auth(d) => write!(f, "exchange auth error: {d}"),
            Self::NotFound(d) => write!(f, "exchange not found: {d}"),
            Self::Uncertain(d) => write!(f, "exchange uncertain outcome: {d}"),
        }
    }
}

impl std::error::Error for ExchangeError {}

/// One public market-data observation for a single symbol on one exchange.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MarketTick {
    /// Exchange identifier, e.g. `"coinbase"` / `"kraken"`.
    pub exchange: String,
    /// Canonical symbol, e.g. `"BTC-USD"`.
    pub symbol: String,
    /// Last trade / mid price in quote currency.
    pub price: f64,
    /// Traded volume (base currency) for the observation window.
    pub volume: f64,
    /// Observation timestamp (exchange or ingest time, UTC).
    pub ts: chrono::DateTime<chrono::Utc>,
}

/// Async interface every exchange feed must implement.
///
/// Implementations must be `Send + Sync` so clients can be shared behind
/// `Arc` across Tokio tasks. No network calls happen in this module.
pub trait ExchangeClient: Send + Sync {
    /// Static exchange identifier, e.g. `"coinbase"`.
    fn name(&self) -> &str;

    /// Fetch the latest tick for `symbol` (e.g. `"BTC-USD"`).
    async fn fetch_tick(&self, symbol: &str) -> Result<MarketTick, ExchangeError>;

    /// Lightweight liveness probe (public endpoint, no auth).
    async fn health(&self) -> Result<(), ExchangeError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_covers_all_variants() {
        let cases = vec![
            (
                ExchangeError::Timeout("read timed out".to_owned()),
                "timeout",
            ),
            (
                ExchangeError::Network("connection reset".to_owned()),
                "network",
            ),
            (
                ExchangeError::RateLimited {
                    detail: "429 slow down".to_owned(),
                    retry_after_secs: Some(30),
                },
                "rate limited",
            ),
            (
                ExchangeError::RateLimited {
                    detail: "slow down".to_owned(),
                    retry_after_secs: None,
                },
                "rate limited",
            ),
            (
                ExchangeError::BadResponse("HTTP 500".to_owned()),
                "bad response",
            ),
            (ExchangeError::Auth("401 bad key".to_owned()), "auth"),
            (
                ExchangeError::NotFound("BTC-USD".to_owned()),
                "not found",
            ),
            (
                ExchangeError::Uncertain("ack unknown".to_owned()),
                "uncertain",
            ),
        ];
        for (err, needle) in cases {
            let rendered = err.to_string();
            assert!(
                rendered.to_lowercase().contains(needle),
                "Display of {err:?} ({rendered}) should contain {needle:?}"
            );
            // std::error::Error must be implemented for `?` interop.
            let _: &dyn std::error::Error = &err;
        }
    }

    #[test]
    fn tick_construction_sets_all_fields() {
        let ts = chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let tick = MarketTick {
            exchange: "kraken".to_owned(),
            symbol: "BTC-USD".to_owned(),
            price: 42_000.5,
            volume: 1.25,
            ts,
        };
        assert_eq!(tick.exchange, "kraken");
        assert_eq!(tick.symbol, "BTC-USD");
        assert!((tick.price - 42_000.5).abs() < f64::EPSILON);
        assert!((tick.volume - 1.25).abs() < f64::EPSILON);
        assert_eq!(tick.ts, ts);
        // Clone + serde round-trip keep the value intact.
        let round_trip: MarketTick =
            serde_json::from_str(&serde_json::to_string(&tick).unwrap()).unwrap();
        assert_eq!(round_trip, tick);
    }
}
