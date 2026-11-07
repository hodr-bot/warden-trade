//! Real-time price feed: Kraken public WebSocket -> server push (SSE).
//!
//! Why this shape: Michel wants true live, not 30s polling. The server holds
//! ONE Kraken WS connection (free, keyless, `wss://ws.kraken.com` ticker) and
//! keeps the latest mark per symbol in memory. Browsers subscribe to
//! `GET /api/live/stream` (Server-Sent Events) and get a JSON snapshot every
//! ~2s: marks + the portfolio block, computed SERVER-side from live fills
//! (financial data always comes from the backend, never computed in JS).
//!
//! [`spawn_feed`] is called once at boot from `main`. If Kraken is
//! unreachable the feed retries with backoff and marks simply stay empty:
//! pages keep working on the 30s HTMX polling fallback.

use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

use futures_util::StreamExt;

/// Our symbols and their Kraken v1 WS pair names.
pub const PAIRS: [(&str, &str); 3] = [
    ("BTC-USD", "XBT/USD"),
    ("ETH-USD", "ETH/USD"),
    ("SOL-USD", "SOL/USD"),
];

/// Latest mark per symbol, written by the feed task, read by SSE.
static MARKS: OnceLock<RwLock<BTreeMap<String, f64>>> = OnceLock::new();

fn marks() -> &'static RwLock<BTreeMap<String, f64>> {
    MARKS.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Current marks (copy). Empty when the feed never connected.
pub fn current_marks() -> BTreeMap<String, f64> {
    marks().read().map(|m| m.clone()).unwrap_or_default()
}

/// Map a Kraken v1 pair name to our symbol. `None` for pairs we don't track.
pub fn kraken_pair_to_symbol(pair: &str) -> Option<&'static str> {
    match pair {
        "XBT/USD" | "XXBTZUSD" => Some("BTC-USD"),
        "ETH/USD" | "XETHZUSD" => Some("ETH-USD"),
        "SOL/USD" | "SOLUSD" => Some("SOL-USD"),
        _ => None,
    }
}

/// Pure ticker parser: Kraken v1 `ticker` event payload ->
/// `(our symbol, last price)`. `None` for anything else (heartbeat,
/// subscription status, unknown pair, malformed).
///
/// Wire shape: `[channel_id, {"c": ["last_trade_price", "lot_volume"]}, "ticker", "XBT/USD"]`.
pub fn parse_ticker(msg: &serde_json::Value) -> Option<(String, f64)> {
    let arr = msg.as_array()?;
    if arr.len() < 4 || arr.get(2)?.as_str()? != "ticker" {
        return None;
    }
    let pair = arr.get(3)?.as_str()?;
    let symbol = kraken_pair_to_symbol(pair)?;
    let last = arr.get(1)?.get("c")?.get(0)?.as_str()?.parse::<f64>().ok()?;
    if !last.is_finite() || last <= 0.0 {
        return None;
    }
    Some((symbol.to_string(), last))
}

/// Record one mark. The single writer is the feed task.
pub fn record_mark(symbol: &str, price: f64) {
    if let Ok(mut m) = marks().write() {
        m.insert(symbol.to_string(), price);
    }
}

/// One feed iteration against `url` (v1 WS API): subscribe, then record
/// ticker marks until the socket dies. Returns when the connection drops so
/// the supervisor can reconnect with backoff.
async fn feed_once(url: &str) {
    use futures_util::SinkExt;
    use tokio_tungstenite::connect_async;
    use tokio_tungstenite::tungstenite::Message;

    let Ok((mut ws, _)) = connect_async(url).await else {
        return;
    };
    let pairs: Vec<&str> = PAIRS.iter().map(|(_, k)| *k).collect();
    let sub = serde_json::json!({
        "event": "subscribe",
        "pair": pairs,
        "subscription": { "name": "ticker" },
    });
    if ws.send(Message::Text(sub.to_string().into())).await.is_err() {
        return;
    }
    while let Some(msg) = ws.next().await {
        let Ok(msg) = msg else { break };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        if let Some((symbol, price)) = parse_ticker(&v) {
            record_mark(&symbol, price);
        }
    }
}

/// Supervisor: runs forever, reconnects with backoff (5s, 15s, 30s, 30s…).
/// Spawned once from `main`; never in tests.
pub fn spawn_feed() {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(5);
        loop {
            feed_once("wss://ws.kraken.com").await;
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
}

/// Portfolio block for the live snapshot, computed server-side from live
/// fills + current marks. Mirrors the dashboard valuation (same FIFO,
/// same public scope as `/`).
async fn portfolio_json(pool: &sqlx::PgPool, marks: &BTreeMap<String, f64>) -> serde_json::Value {
    let fills: Vec<(String, String, f64, f64)> = sqlx::query_as(
        "SELECT symbol, side, qty, price FROM paper_orders ORDER BY created_at ASC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut by_symbol: BTreeMap<String, Vec<(String, f64, f64)>> = BTreeMap::new();
    for (symbol, side, qty, price) in &fills {
        by_symbol
            .entry(symbol.clone())
            .or_default()
            .push((side.clone(), *qty, *price));
    }
    let (realized, closed, _wins, unrealized, open_value) =
        crate::dashboard::value_positions(&by_symbol, marks);
    let eur_rate = crate::dashboard::fetch_usd_eur().await;
    let total = realized + unrealized;
    let capital = crate::dashboard::paper_starting_capital_usd() + total;
    serde_json::json!({
        "capital": crate::dashboard::fmt_usd(capital),
        "capital_sub": crate::dashboard::fmt_eur(capital, eur_rate),
        "capital_class": crate::dashboard::delta_class(total),
        "value": crate::dashboard::fmt_usd(total),
        "value_sub": format!("Mark-to-market · {}", crate::dashboard::fmt_eur(total, eur_rate)),
        "class": crate::dashboard::delta_class(total),
        "rclass": crate::dashboard::delta_class(realized),
        "uclass": crate::dashboard::delta_class(unrealized),
        "realized": crate::dashboard::fmt_usd(realized),
        "realized_sub": crate::dashboard::fmt_eur(realized, eur_rate),
        "unrealized": crate::dashboard::fmt_usd(unrealized),
        "unrealized_sub": crate::dashboard::fmt_eur(unrealized, eur_rate),
        "open_value": crate::dashboard::fmt_usd(open_value),
        "closed": closed,
    })
}

/// `GET /api/live/stream`: SSE snapshot every 2s (marks + portfolio).
///
/// Same public scope as `/`: marks are public market data and the paper
/// portfolio is already shown unauthenticated on the dashboard.
pub async fn stream_handler(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
) -> axum::response::Sse<impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>> {
    let pool = state.pool.clone();
    let stream = futures_util::stream::unfold(0u64, move |tick| {
        let pool = pool.clone();
        async move {
            if tick > 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            let marks = current_marks();
            let data = serde_json::json!({
                "marks": marks,
                "portfolio": portfolio_json(&pool, &marks).await,
            });
            let event = axum::response::sse::Event::default().data(data.to_string());
            Some((Ok::<_, std::convert::Infallible>(event), tick + 1))
        }
    });
    axum::response::Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticker(pair: &str, last: &str) -> serde_json::Value {
        serde_json::json!([123, { "c": [last, "0.5"] }, "ticker", pair])
    }

    #[test]
    fn parses_btc_ticker_to_our_symbol() {
        let (sym, price) = parse_ticker(&ticker("XBT/USD", "75918.1")).expect("parses");
        assert_eq!(sym, "BTC-USD");
        assert!((price - 75918.1).abs() < 1e-9);
    }

    #[test]
    fn maps_all_tracked_pairs() {
        assert_eq!(kraken_pair_to_symbol("XBT/USD"), Some("BTC-USD"));
        assert_eq!(kraken_pair_to_symbol("ETH/USD"), Some("ETH-USD"));
        assert_eq!(kraken_pair_to_symbol("SOL/USD"), Some("SOL-USD"));
        assert_eq!(kraken_pair_to_symbol("DOGE/USD"), None);
    }

    #[test]
    fn rejects_noise_and_garbage() {
        // Heartbeat / subscription status / system messages.
        assert!(parse_ticker(&serde_json::json!({"event": "heartbeat"})).is_none());
        assert!(parse_ticker(&serde_json::json!({"event":"subscriptionStatus","status":"subscribed"})).is_none());
        // Untracked pair.
        assert!(parse_ticker(&ticker("DOGE/USD", "0.2")).is_none());
        // Bad price.
        assert!(parse_ticker(&ticker("XBT/USD", "abc")).is_none());
        assert!(parse_ticker(&ticker("XBT/USD", "-5")).is_none());
        // Wrong shape.
        assert!(parse_ticker(&serde_json::json!([1, 2, 3])).is_none());
    }
}
