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

/// One tracked asset on both public feeds: our symbol, Coinbase spot
/// product, Kraken ticker pair.
const ASSETS: [(&str, &str, &str); 3] = [
    ("BTC-USD", "BTC-USD", "XBTUSD"),
    ("ETH-USD", "ETH-USD", "ETHUSD"),
    ("SOL-USD", "SOL-USD", "SOLUSD"),
];

/// Build stub snapshots (price 0) for every tracked asset.
///
/// Paper trading only: no network calls, no secrets.
/// Used as the degraded fallback when live feeds are unreachable.
pub fn paper_snapshot() -> Vec<MarketSnapshot> {
    let retrieved_at = chrono::Utc::now().to_rfc3339();
    let mut out = Vec::with_capacity(ASSETS.len() * 2);
    for (symbol, _, krak) in ASSETS {
        out.push(MarketSnapshot {
            exchange: "coinbase".to_string(),
            symbol: symbol.to_string(),
            price_f64: 0.0,
            retrieved_at: retrieved_at.clone(),
        });
        out.push(MarketSnapshot {
            exchange: "kraken".to_string(),
            symbol: krak.to_string(),
            price_f64: 0.0,
            retrieved_at: retrieved_at.clone(),
        });
    }
    out
}

/// Fetch live public prices when reachable, degrading each leg to `0.0`.
///
/// Shared by the JSON API and the HTML page: tries every tracked asset on
/// Coinbase spot and Kraken ticker concurrently (read-only, no auth,
/// no secrets). `retrieved_at` is always backend time.
///
/// Kraken legs additionally degrade to the live WS mark (when the feed
/// task already learned it): REST and WS agree to the cent most of the
/// time, and a WS mark beats a `0.0`.
pub async fn fetch_live() -> Vec<MarketSnapshot> {
    let retrieved_at = chrono::Utc::now().to_rfc3339();
    let coinbase = crate::coinbase::CoinbaseClient::new();
    let kraken = crate::kraken::KrakenClient::new();
    let (cb_btc, cb_eth, cb_sol, kr_btc, kr_eth, kr_sol) = tokio::join!(
        coinbase.fetch_spot_price("BTC-USD"),
        coinbase.fetch_spot_price("ETH-USD"),
        coinbase.fetch_spot_price("SOL-USD"),
        kraken.fetch_last_price("XBTUSD"),
        kraken.fetch_last_price("ETHUSD"),
        kraken.fetch_last_price("SOLUSD")
    );
    let cb = [cb_btc, cb_eth, cb_sol];
    let kr = [kr_btc, kr_eth, kr_sol];
    let mut out = Vec::with_capacity(ASSETS.len() * 2);
    for (i, (symbol, _, krak)) in ASSETS.iter().enumerate() {
        let cb_price = match &cb[i] {
            Ok(p) => *p,
            Err(e) => {
                tracing::warn!("coinbase {symbol} degraded: {e}");
                0.0
            }
        };
        out.push(MarketSnapshot {
            exchange: "coinbase".to_string(),
            symbol: symbol.to_string(),
            price_f64: cb_price,
            retrieved_at: retrieved_at.clone(),
        });
        let kr_price = match &kr[i] {
            Ok(p) => *p,
            Err(e) => {
                tracing::warn!("kraken {krak} degraded: {e}");
                crate::live::current_marks().get(*symbol).copied().unwrap_or(0.0)
            }
        };
        out.push(MarketSnapshot {
            exchange: "kraken".to_string(),
            symbol: krak.to_string(),
            price_f64: kr_price,
            retrieved_at: retrieved_at.clone(),
        });
    }
    out
}

/// Axum handler returning live prices when reachable.
///
/// Requires login ([`Tenant`]: Bearer header or session cookie). Tries
/// Coinbase `BTC-USD` spot and Kraken `XBTUSD` ticker concurrently
/// (read-only, no secrets). Any feed failure degrades that leg to `0.0`
/// with a warning log; `retrieved_at` is always backend time.
pub async fn markets_handler(_tenant: crate::state::Tenant) -> Json<Vec<MarketSnapshot>> {
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
/// Requires login ([`AuthPage`] redirects browsers to `/login`).
/// Degrades to the zero-price stub table when both feeds are unreachable.
pub async fn markets_page_handler(
    _page: crate::state::AuthPage,
) -> axum::response::Html<String> {
    let snapshots = fetch_live().await;
    let template = MarketsTemplate {
        snapshots,
        generated_at: crate::dashboard::short_now(),
    };
    axum::response::Html(template.render().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_covers_every_asset_on_both_feeds() {
        let snapshots = paper_snapshot();
        assert_eq!(snapshots.len(), ASSETS.len() * 2);
        for (symbol, _, krak) in ASSETS {
            assert!(snapshots.iter().any(|s| s.exchange == "coinbase" && s.symbol == symbol));
            assert!(snapshots.iter().any(|s| s.exchange == "kraken" && s.symbol == krak));
        }
    }

    #[tokio::test]
    async fn handler_returns_every_asset_on_both_feeds() {
        let tenant = crate::state::Tenant {
            tenant_id: "00000000-0000-0000-0000-000000000001".to_string(),
            user_id: "u".to_string(),
        };
        let Json(snapshots) = markets_handler(tenant).await;
        assert_eq!(snapshots.len(), ASSETS.len() * 2);
    }
}
