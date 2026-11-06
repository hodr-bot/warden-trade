//! Coinbase read-only public feed client (paper trading only).
//!
//! Public spot price endpoint (no auth, no secrets):
//! `GET {base_url}/v2/prices/{symbol}/spot`, e.g.
//! `https://api.coinbase.com/v2/prices/BTC-USD/spot`.
//!
//! Response shape (Coinbase v2 API, `data.amount` is a decimal string):
//! ```json
//! {"data": {"base": "BTC", "currency": "USD", "amount": "64234.12"}}
//! ```
//!
//! No external network calls in unit tests (one test hits the
//! unreachable loopback `http://127.0.0.1:9`, which fails locally
//! without internet). No secrets. Read-only public feed only.

use reqwest::Client;
use std::time::Duration;

use crate::exchanges::ExchangeError;

/// Minimal Coinbase client for the public spot-price feed.
#[derive(Debug, Clone)]
pub struct CoinbaseClient {
    base_url: String,
    http: Client,
}

impl CoinbaseClient {
    /// Default client pointing at the public Coinbase API.
    pub fn new() -> Self {
        Self {
            base_url: "https://api.coinbase.com".to_owned(),
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

    /// Build the public spot-price URL for a pair like `"BTC-USD"`.
    pub fn spot_url(&self, symbol: &str) -> String {
        format!(
            "{}/v2/prices/{}/spot",
            self.base_url.trim_end_matches('/'),
            symbol
        )
    }

    /// Fetch the spot price for a pair like `"BTC-USD"` via `GET spot_url`.
    ///
    /// Read-only public feed (no auth, no secrets). Applies a 10 second
    /// timeout and maps failures onto [`ExchangeError`]: timeouts to
    /// `Timeout`, connection failures to `Network`, 429 to `RateLimited`,
    /// 401/403 to `Auth`, 404 to `NotFound`, other bad statuses to
    /// `BadResponse`. The success body is parsed with
    /// [`Self::parse_spot_response`].
    pub async fn fetch_spot_price(&self, symbol: &str) -> Result<f64, ExchangeError> {
        let url = self.spot_url(symbol);
        let request = self.http.get(&url).timeout(Duration::from_secs(10));
        let resp = tokio::time::timeout(Duration::from_secs(10), request.send())
            .await
            .map_err(|_| {
                ExchangeError::Timeout(format!("coinbase spot request timed out: {url}"))
            })?
            .map_err(|e| {
                if e.is_timeout() {
                    ExchangeError::Timeout(format!("coinbase spot request timed out: {e}"))
                } else if e.is_connect() {
                    ExchangeError::Network(format!("coinbase spot connection failed: {e}"))
                } else {
                    ExchangeError::Network(format!("coinbase spot request failed: {e}"))
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
                detail: format!("coinbase spot rate limited: HTTP {status} for {url}"),
                retry_after_secs,
            });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(ExchangeError::Auth(format!(
                "coinbase spot auth error: HTTP {status} for {url}"
            )));
        }
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ExchangeError::NotFound(format!(
                "coinbase spot not found: HTTP {status} for {url}"
            )));
        }
        if !status.is_success() {
            return Err(ExchangeError::BadResponse(format!(
                "coinbase spot bad status: HTTP {status} for {url}"
            )));
        }

        let body = tokio::time::timeout(Duration::from_secs(10), resp.text())
            .await
            .map_err(|_| {
                ExchangeError::Timeout(format!("coinbase spot body timed out: {url}"))
            })?
            .map_err(|e| {
                if e.is_timeout() {
                    ExchangeError::Timeout(format!("coinbase spot body timed out: {e}"))
                } else {
                    ExchangeError::Network(format!("coinbase spot body read failed: {e}"))
                }
            })?;
        Self::parse_spot_response(&body).map_err(ExchangeError::BadResponse)
    }

    /// Parse a Coinbase v2 spot-price JSON payload into an `f64` price.
    ///
    /// Expected shape: `{"data": {"base": "BTC", "currency": "USD", "amount": "123.45"}}`.
    /// Pure function: no network, no secrets.
    pub fn parse_spot_response(body: &str) -> Result<f64, String> {
        #[derive(serde::Deserialize)]
        struct SpotData {
            amount: String,
        }
        #[derive(serde::Deserialize)]
        struct SpotResponse {
            data: SpotData,
        }

        let parsed: SpotResponse =
            serde_json::from_str(body).map_err(|e| format!("invalid coinbase spot JSON: {e}"))?;
        parsed
            .data
            .amount
            .parse::<f64>()
            .map_err(|e| format!("invalid coinbase spot amount: {e}"))
    }
}

impl Default for CoinbaseClient {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_url_builds_public_spot_path() {
        let client = CoinbaseClient::with_base_url("https://api.coinbase.com");
        assert_eq!(
            client.spot_url("BTC-USD"),
            "https://api.coinbase.com/v2/prices/BTC-USD/spot"
        );

        // Trailing slash on base_url must not produce a double slash.
        let slashed = CoinbaseClient::with_base_url("https://api.coinbase.com/");
        assert_eq!(
            slashed.spot_url("ETH-USD"),
            "https://api.coinbase.com/v2/prices/ETH-USD/spot"
        );

        // Default constructor targets the public feed.
        let default_client = CoinbaseClient::new();
        assert_eq!(
            default_client.spot_url("BTC-USD"),
            "https://api.coinbase.com/v2/prices/BTC-USD/spot"
        );
    }

    #[test]
    fn parse_spot_response_reads_static_sample() {
        // Static sample of Coinbase v2 `GET /v2/prices/BTC-USD/spot`. No network.
        let sample = r#"{"data":{"base":"BTC","currency":"USD","amount":"64234.12"}}"#;
        let price = CoinbaseClient::parse_spot_response(sample).expect("sample must parse");
        assert!((price - 64234.12).abs() < 1e-9);

        // Malformed payloads are rejected (not silently defaulted).
        assert!(CoinbaseClient::parse_spot_response("not json").is_err());
        assert!(CoinbaseClient::parse_spot_response(
            r#"{"data":{"base":"BTC","currency":"USD","amount":"abc"}}"#
        )
        .is_err());
    }

    #[tokio::test]
    async fn fetch_spot_price_unreachable_base_returns_exchange_error() {
        // Unroutable loopback port: connection refused locally, no external network.
        let client = CoinbaseClient::with_base_url("http://127.0.0.1:9");
        let err = client
            .fetch_spot_price("BTC-USD")
            .await
            .expect_err("unreachable base_url must fail");
        let _: &ExchangeError = &err;
    }
}
