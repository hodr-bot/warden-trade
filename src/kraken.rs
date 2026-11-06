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
//! No network calls in unit tests. No secrets. Read-only public feed only.

use std::collections::HashMap;

use reqwest::Client;

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
}
