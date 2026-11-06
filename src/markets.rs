use axum::Json;
use serde::Serialize;

/// Point-in-time market price snapshot (paper / stub, no network).
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

/// Axum handler returning the paper snapshots as JSON.
pub async fn markets_handler() -> Json<Vec<MarketSnapshot>> {
    Json(paper_snapshot())
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
