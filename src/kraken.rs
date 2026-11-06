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
use std::collections::BTreeMap;

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

// ---------------------------------------------------------------------------
// Private authenticated API (live wallet readout). Read-only for now:
// `Balance` only. `AddOrder` is NOT wired (ships tomorrow behind
// `LIVE_TRADING=true` + cap, after explicit validation).
//
// Signing per the official Kraken Spot REST auth guide
// (`docs.kraken.com/exchange/guides/rest/authentication`):
// `API-Sign = base64(HMAC-SHA512(key = base64decode(secret),`
// `msg = uri_path + SHA256(nonce + POST_data)))`, headers `API-Key` /
// `API-Sign`. Credentials ONLY from server env (`KRAKEN_API_KEY`,
// `KRAKEN_API_SECRET`): never in the DB, never in logs, never frontend.
// ---------------------------------------------------------------------------

/// Server-side Kraken credentials. The secret lives in memory only for
/// the duration of a request and is never logged.
pub struct KrakenCreds {
    pub key: String,
    pub secret_b64: String,
}

impl KrakenCreds {
    /// `None` when env is missing: live features report "not configured".
    pub fn from_env() -> Option<Self> {
        let key = std::env::var("KRAKEN_API_KEY").ok()?;
        let secret_b64 = std::env::var("KRAKEN_API_SECRET").ok()?;
        if key.is_empty() || secret_b64.is_empty() {
            return None;
        }
        Some(Self { key, secret_b64 })
    }
}

/// Pure Kraken signature. `post_data` is the exact url-encoded body
/// (`nonce=...&...`); `path` starts at `/0/private`.
pub fn kraken_sign(path: &str, nonce: &str, post_data: &str, secret_b64: &str) -> Result<String, String> {
    use base64::Engine;
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256, Sha512};

    let secret = base64::engine::general_purpose::STANDARD
        .decode(secret_b64.trim())
        .map_err(|e| format!("bad secret base64: {e}"))?;
    let mut sha = Sha256::new();
    sha.update(nonce.as_bytes());
    sha.update(post_data.as_bytes());
    let sha_sum = sha.finalize();

    let mut msg = Vec::with_capacity(path.len() + sha_sum.len());
    msg.extend_from_slice(path.as_bytes());
    msg.extend_from_slice(&sha_sum);
    let mut mac =
        Hmac::<Sha512>::new_from_slice(&secret).map_err(|e| format!("hmac init: {e}"))?;
    mac.update(&msg);
    Ok(base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes()))
}

/// Monotonic-ish nonce: microseconds since epoch (Kraken rejects reused nonces).
pub fn kraken_nonce() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros().to_string())
        .unwrap_or_else(|_| "1".to_string())
}

async fn kraken_private_post(
    creds: &KrakenCreds,
    path: &str,
    params: &[(&str, &str)],
    client: &reqwest::Client,
) -> Result<serde_json::Value, String> {
    let nonce = kraken_nonce();
    let mut body = format!("nonce={nonce}");
    for (k, v) in params {
        body.push('&');
        body.push_str(k);
        body.push('=');
        body.push_str(v);
    }
    let sig = kraken_sign(path, &nonce, &body, &creds.secret_b64)?;
    let url = format!("https://api.kraken.com{path}");
    let resp = client
        .post(&url)
        .header("API-Key", &creds.key)
        .header("API-Sign", sig)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .map_err(|e| format!("kraken http: {e}"))?;
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("kraken json: {e}"))?;
    let empty = v
        .get("error")
        .and_then(|e| e.as_array())
        .map(|a| a.is_empty())
        .unwrap_or(false);
    if !empty {
        return Err(format!("kraken error: {}", v.get("error").unwrap_or(&v)));
    }
    Ok(v.get("result").cloned().unwrap_or(serde_json::Value::Null))
}

/// Nonzero balances by asset (e.g. `XXBT`, `ZEUR`): the "real wallet" readout.
pub async fn kraken_balance(creds: &KrakenCreds) -> Result<BTreeMap<String, f64>, String> {
    use std::collections::BTreeMap;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("http client: {e}"))?;
    let result = kraken_private_post(creds, "/0/private/Balance", &[], &client).await?;
    let mut out = BTreeMap::new();
    if let Some(obj) = result.as_object() {
        for (asset, v) in obj {
            let amt: f64 = v.as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
            if amt > 0.0 {
                out.insert(asset.clone(), amt);
            }
        }
    }
    Ok(out)
}

/// `GET /api/live/status`: real-wallet readout for the solo admin.
///
/// Requires `Authorization: Bearer *** via [`crate::auth::Tenant`].
/// No key material ever leaves the server: only asset amounts.
pub async fn live_status_handler(
    axum::extract::State(_state): axum::extract::State<crate::state::AppState>,
    _tenant: crate::state::Tenant,
) -> axum::Json<serde_json::Value> {
    use std::collections::BTreeMap;
    let (configured, balances, error): (bool, BTreeMap<String, f64>, Option<String>) =
        match KrakenCreds::from_env() {
            None => (false, BTreeMap::new(), None),
            Some(creds) => match kraken_balance(&creds).await {
                Ok(b) => (true, b, None),
                Err(e) => (true, BTreeMap::new(), Some(e)),
            },
        };
    axum::Json(serde_json::json!({
        "configured": configured,
        "balances": balances,
        "error": error,
    }))
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

    #[test]
    fn official_signature_vector() {
        // From Kraken's own Spot REST auth guide (authentication page).
        let secret = "kQH5HW/8p1uGOVjbgWA7FunAmGO8lsSUXNsu3eow76sz84Q18fWxnyRzBHCd3pd5nE9qa99HAZtuZuj6F1huXg==";
        let nonce = "1616492376594";
        let body = "nonce=1616492376594&ordertype=limit&pair=XBTUSD&price=37500&type=buy&volume=1.25";
        let sig = kraken_sign("/0/private/AddOrder", nonce, body, secret).expect("signs");
        assert_eq!(
            sig,
            "4/dpxb3iT4tp/ZCVEwSnEsLxx0bqyhLpdfOpc6fn7OR8+UClSV5n9E6aSS8MPtnRfp32bAb0nmbRn6H8ndwLUQ=="
        );
    }

    #[test]
    fn bad_secret_is_an_error_not_a_panic() {
        assert!(kraken_sign("/0/private/Balance", "1", "nonce=1", "!!!").is_err());
    }

    #[test]
    fn creds_absent_without_env() {
        std::env::remove_var("KRAKEN_API_KEY");
        std::env::remove_var("KRAKEN_API_SECRET");
        assert!(KrakenCreds::from_env().is_none());
    }
}
