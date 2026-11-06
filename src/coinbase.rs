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
//! No network calls in unit tests. No secrets. Read-only public feed only.

use reqwest::Client;

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
}
