use askama::Template;
use axum::Json;
use serde::Serialize;

/// Point-in-time market price snapshot (paper trading only, read-only feeds).
#[derive(Debug, Clone, Serialize)]
pub struct MarketSnapshot {
    pub exchange: String,
    pub symbol: String,
    pub price_f64: f64,
    pub retrieved_at: String,
}

/// Build two stub snapshots with price 0 and current timestamp.
///
/// Paper trading only: no network calls, no secrets.
/// Used as the degraded fallback when live feeds are unreachable.
pub fn paper_snapshot() -> Vec<MarketSnapshot> {
    let retrieved_at = chrono::Utc::now().to_rfc3339();
    vec![
        MarketSnapshot {
            exchange: "coinbase".to_string(),
            symbol: "BTC-USD".to_string(),
            price_f64: 0.0,
            retrieved_at: retrieved_at.clone(),
        },
        MarketSnapshot {
            exchange: "kraken".to_string(),
            symbol: "XBTUSD".to_string(),
            price_f64: 0.0,
            retrieved_at,
        },
    ]
}

/// Fetch live public prices when reachable, degrading each leg to `0.0`.
///
/// Shared by the JSON API and the HTML page: tries Coinbase `BTC-USD`
/// spot and Kraken `XBTUSD` ticker concurrently (read-only, no auth,
/// no secrets). `retrieved_at` is always backend time.
pub async fn fetch_live() -> Vec<MarketSnapshot> {
    let retrieved_at = chrono::Utc::now().to_rfc3339();
    let coinbase = crate::coinbase::CoinbaseClient::new();
    let kraken = crate::kraken::KrakenClient::new();
    let (cb, kr) = tokio::join!(
        coinbase.fetch_spot_price("BTC-USD"),
        kraken.fetch_last_price("XBTUSD")
    );
    let cb_price = match cb {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("coinbase feed degraded: {e}");
            0.0
        }
    };
    let kr_price = match kr {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("kraken feed degraded: {e}");
            0.0
        }
    };
    vec![
        MarketSnapshot {
            exchange: "coinbase".to_string(),
            symbol: "BTC-USD".to_string(),
            price_f64: cb_price,
            retrieved_at: retrieved_at.clone(),
        },
        MarketSnapshot {
            exchange: "kraken".to_string(),
            symbol: "XBTUSD".to_string(),
            price_f64: kr_price,
            retrieved_at,
        },
    ]
}

/// Axum handler returning live public prices when reachable.
///
/// Tries Coinbase `BTC-USD` spot and Kraken `XBTUSD` ticker concurrently
/// (read-only, no auth, no secrets). Any feed failure degrades that leg
/// to `0.0` with a warning log; `retrieved_at` is always backend time.
pub async fn markets_handler() -> Json<Vec<MarketSnapshot>> {
    Json(fetch_live().await)
}

/// Askama template for the markets HTML page (same live data as the API).
#[derive(askama::Template)]
#[template(path = "markets.html")]
pub struct MarketsTemplate {
    pub snapshots: Vec<MarketSnapshot>,
    pub generated_at: String,
}

/// Server-rendered markets page: live prices in HTML, HTMX refresh.
///
/// Public (same-origin solo-admin page); the JSON API stays JWT-gated
/// where it needs tenant scope. Degrades to the zero-price stub table
/// when both feeds are unreachable.
pub async fn markets_page_handler() -> axum::response::Html<String> {
    let snapshots = fetch_live().await;
    let template = MarketsTemplate {
        snapshots,
        generated_at: chrono::Utc::now().to_rfc3339(),
    };
    axum::response::Html(template.render().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_count_is_two() {
        let snapshots = paper_snapshot();
        assert_eq!(snapshots.len(), 2);
    }

    #[tokio::test]
    async fn handler_returns_two_entries() {
        let Json(snapshots) = markets_handler().await;
        assert_eq!(snapshots.len(), 2);
    }
}
