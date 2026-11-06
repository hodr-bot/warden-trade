//! Kraken read-only public feed client (paper trading only).
//!
//! Public ticker endpoint (no auth, no secrets):
//! `GET {base_url}/0/public/Ticker?pair={pair}`, e.g.
//! `https://api.kraken.com/0/public/Ticker?pair=XBTUSD`.
//!
//! Response shape (Kraken API, `result.<pair>.c[0]` is last-price string):
//! ```json
//! {"error":[],"result":{"XXBTZUSD":{"a":["1","2"],"b":["1","2"],"c":["64234.1","0.5"]}}}
//! ```
//!
//! No external network calls in unit tests (one test hits the
//! unreachable loopback `http://127.0.0.1:9`, which fails locally
//! without internet). No secrets. Read-only public feed only.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;

use crate::exchanges::ExchangeError;

/// Minimal Kraken client for the public ticker feed.
#[derive(Debug, Clone)]
pub struct KrakenClient {
    base_url: String,
    http: Client,
}

impl KrakenClient {
    /// Default client pointing at the public Kraken API.
    pub fn new() -> Self {
        Self {
            base_url: "https://api.kraken.com".to_owned(),
            http: Client::new(),
        }
    }

    /// Constructor with an explicit base URL (used by tests / stubs).
    pub fn with_base_url(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: Client::new(),
        }
    }

    /// Build the public ticker URL for a pair like `"XBTUSD"`.
    pub fn ticker_url(&self, pair: &str) -> String {
        format!(
            "{}/0/public/Ticker?pair={}",
            self.base_url.trim_end_matches('/'),
            pair
        )
    }

    /// Fetch the last price for a pair like `"XBTUSD"` via `GET ticker_url`.
    ///
    /// Read-only public feed (no auth, no secrets). Applies a 10 second
    /// timeout and maps failures onto [`ExchangeError`]: timeouts to
    /// `Timeout`, connection failures to `Network`, 429 to `RateLimited`,
    /// 401/403 to `Auth`, 404 to `NotFound`, other bad statuses to
    /// `BadResponse`. The success body is parsed with
    /// [`Self::parse_ticker_response`].
    pub async fn fetch_last_price(&self, pair: &str) -> Result<f64, ExchangeError> {
        let url = self.ticker_url(pair);
        let request = self.http.get(&url).timeout(Duration::from_secs(10));
        let resp = tokio::time::timeout(Duration::from_secs(10), request.send())
            .await
            .map_err(|_| {
                ExchangeError::Timeout(format!("kraken ticker request timed out: {url}"))
            })?
            .map_err(|e| {
                if e.is_timeout() {
                    ExchangeError::Timeout(format!("kraken ticker request timed out: {e}"))
                } else if e.is_connect() {
                    ExchangeError::Network(format!("kraken ticker connection failed: {e}"))
                } else {
                    ExchangeError::Network(format!("kraken ticker request failed: {e}"))
                }
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after_secs = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok());
            return Err(ExchangeError::RateLimited {
                detail: format!("kraken ticker rate limited: HTTP {status} for {url}"),
                retry_after_secs,
            });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(ExchangeError::Auth(format!(
                "kraken ticker auth error: HTTP {status} for {url}"
            )));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ExchangeError::NotFound(format!(
                "kraken ticker not found: HTTP {status} for {url}"
            )));
        }
        if !status.is_success() {
            return Err(ExchangeError::BadResponse(format!(
                "kraken ticker bad status: HTTP {status} for {url}"
            )));
        }

        let body = tokio::time::timeout(Duration::from_secs(10), resp.text())
            .await
            .map_err(|_| {
                ExchangeError::Timeout(format!("kraken ticker body timed out: {url}"))
            })?
            .map_err(|e| {
                if e.is_timeout() {
                    ExchangeError::Timeout(format!("kraken ticker body timed out: {e}"))
                } else {
                    ExchangeError::Network(format!("kraken ticker body read failed: {e}"))
                }
            })?;
        Self::parse_ticker_response(&body).map_err(ExchangeError::BadResponse)
    }

    /// Parse a Kraken ticker JSON payload into an `f64` last price.
    ///
    /// Expected shape: `{"error":[],"result":{"<pair>":{"c":["<price>","<lot>"],…}}}`.
    /// Takes `c[0]` of the first entry in `result` so Kraken's pair
    /// aliasing (`XBTUSD` -> `XXBTZUSD`) does not matter.
    /// Pure function: no network, no secrets.
    pub fn parse_ticker_response(body: &str) -> Result<f64, String> {
        #[derive(serde::Deserialize)]
        struct TickerEntry {
            c: Vec<String>,
        }
        #[derive(serde::Deserialize)]
        struct TickerResponse {
            error: Vec<String>,
            result: HashMap<String, TickerEntry>,
        }

        let parsed: TickerResponse =
            serde_json::from_str(body).map_err(|e| format!("invalid kraken ticker JSON: {e}"))?;
        if !parsed.error.is_empty() {
            return Err(format!("kraken ticker error: {:?}", parsed.error));
        }
        let entry = parsed
            .result
            .values()
            .next()
            .ok_or_else(|| "kraken ticker result is empty".to_owned())?;
        let last = entry
            .c
            .first()
            .ok_or_else(|| "kraken ticker missing last price (c[0])".to_owned())?;
        last
            .parse::<f64>()
            .map_err(|e| format!("invalid kraken last price: {e}"))
    }
}

impl Default for KrakenClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticker_url_builds_public_ticker_path() {
        let client = KrakenClient::with_base_url("https://api.kraken.com");
        assert_eq!(
            client.ticker_url("XBTUSD"),
            "https://api.kraken.com/0/public/Ticker?pair=XBTUSD"
        );

        // Trailing slash on base_url must not produce a double slash.
        let slashed = KrakenClient::with_base_url("https://api.kraken.com/");
        assert_eq!(
            slashed.ticker_url("ETHUSD"),
            "https://api.kraken.com/0/public/Ticker?pair=ETHUSD"
        );

        // Default constructor targets the public feed.
        let default_client = KrakenClient::new();
        assert_eq!(
            default_client.ticker_url("XBTUSD"),
            "https://api.kraken.com/0/public/Ticker?pair=XBTUSD"
        );
    }

    #[test]
    fn parse_ticker_response_reads_static_sample() {
        // Static sample of Kraken `GET /0/public/Ticker?pair=XBTUSD`. No network.
        let sample = r#"{"error":[],"result":{"XXBTZUSD":{"a":["64240.0","1","1"],"b":["64230.0","1","1"],"c":["64234.10","0.5"],"v":["1","2"],"p":["1","2"],"t":[1,2],"l":["1","2"],"h":["1","2"],"o":"64000.0"}}}"#;
        let price = KrakenClient::parse_ticker_response(sample).expect("sample must parse");
        assert!((price - 64234.10).abs() < 1e-9);

        // Malformed / error payloads are rejected (not silently defaulted).
        assert!(KrakenClient::parse_ticker_response("not json").is_err());
        assert!(KrakenClient::parse_ticker_response(r#"{"error":["EQuery:Unknown asset pair"],"result":{}}"#).is_err());
        assert!(KrakenClient::parse_ticker_response(r#"{"error":[],"result":{}}"#).is_err());
    }

    #[tokio::test]
    async fn fetch_last_price_unreachable_base_returns_exchange_error() {
        // Unroutable loopback port: connection refused locally, no external network.
        let client = KrakenClient::with_base_url("http://127.0.0.1:9");
        let err = client
            .fetch_last_price("XBTUSD")
            .await
            .expect_err("unreachable base_url must fail");
        let _: &ExchangeError = &err;
    }
}
