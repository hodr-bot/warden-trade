//! Autonomous paper-trading scheduler (paper only, never real funds).
//!
//! Every [`interval`](scheduler_interval_secs) the scheduler runs one
//! [`orchestrator`](crate::orchestrator) cycle per configured symbol
//! ([`scheduler_symbols`], default BTC/ETH/SOL vs USD) on live Coinbase
//! prices, enriched with market context (news sentiment, Fear & Greed,
//! per-asset funding rates), and, when strategy and risk agree on a
//! `buy`/`sell` intent, places exactly one paper order through the same
//! risk-gated [`place_paper_order`](crate::orders::place_paper_order)
//! path as the manual API.
//!
//! Position discipline: a signal matching the side of the tenant's most
//! recent order for the symbol is skipped (`already positioned`), so the
//! bot alternates long/flat instead of stacking. The idempotency key
//! pins each cycle slot, so a retried cycle can never double-place.
//! `SCHEDULER_ENABLED=false` disables the loop; the kill switch in
//! `risk_limits` stays the emergency brake (checked on every order).

use crate::news::{combine_sentiment, fetch_fear_greed, fetch_funding, fetch_sentiment};
use crate::orchestrator::{
    live_closes_for, record_decisions, run_cycle_with_context, sized_qty, MarketContext,
};
use crate::state::AppState;

/// Scheduler tick interval, read from `SCHEDULER_INTERVAL_SECS` (default 300s).
pub fn scheduler_interval_secs() -> u64 {
    std::env::var("SCHEDULER_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v: &u64| *v >= 30)
        .unwrap_or(300)
}

/// Whether the autonomous loop runs (`SCHEDULER_ENABLED`, default true).
pub fn scheduler_enabled() -> bool {
    std::env::var("SCHEDULER_ENABLED")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true)
}

/// Symbols traded autonomously, from `SCHEDULER_SYMBOLS`
/// (comma-separated, default BTC/ETH/SOL vs USD).
pub fn scheduler_symbols() -> Vec<String> {
    let raw = std::env::var("SCHEDULER_SYMBOLS")
        .unwrap_or_else(|_| "BTC-USD,ETH-USD,SOL-USD".to_string());
    let symbols: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_uppercase())
        .filter(|s| !s.is_empty())
        .collect();
    if symbols.is_empty() {
        vec!["BTC-USD".to_string()]
    } else {
        symbols
    }
}

/// Oldest tenant id (solo-admin first). The scheduler trades for the
/// tenant the login flow provisions; missing DB degrades to `None`
/// (cycle skipped, retried next tick).
async fn solo_tenant(pool: &sqlx::PgPool) -> Option<uuid::Uuid> {
    let row: Option<(uuid::Uuid,)> =
        sqlx::query_as("SELECT id FROM tenants ORDER BY created_at ASC LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()?;
    row.map(|r| r.0)
}

/// Side of the tenant's most recent order for `symbol`, if any.
async fn last_side(pool: &sqlx::PgPool, tenant_id: uuid::Uuid, symbol: &str) -> Option<String> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT side FROM paper_orders WHERE tenant_id = $1 AND symbol = $2 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant_id)
    .bind(symbol)
    .fetch_optional(pool)
    .await
    .ok()?;
    row.map(|r| r.0)
}

/// Open long on a symbol from fills: `(qty, average entry price)`.
///
/// `(0, 0)` when flat or net-short. Used to forbid short selling:
/// a `sell` signal with nothing held is skipped, never executed.
async fn long_held(pool: &sqlx::PgPool, tenant_id: uuid::Uuid, symbol: &str) -> (f64, f64) {
    let fills: Vec<(String, f64, f64)> = sqlx::query_as(
        "SELECT side, qty, price FROM paper_orders WHERE tenant_id = $1 AND symbol = $2 ORDER BY created_at ASC",
    )
    .bind(tenant_id)
    .bind(symbol)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    crate::dashboard::long_position(&fills)
}

/// One autonomous tick: cycle + optional single paper order.
///
/// Returns a short human-readable summary for the logs. Never panics;
/// feed or DB failures degrade to hold/skip, never to a trade.
pub async fn tick(state: &AppState) -> String {
    let tenant_id = match solo_tenant(&state.pool).await {
        Some(id) => id,
        None => return "tick skipped: no tenant".to_string(),
    };
    // Market-wide context, fetched once per tick and shared by every symbol.
    let rss = fetch_sentiment().await;
    let fg = fetch_fear_greed().await;
    let sentiment = combine_sentiment(rss, fg);
    let mut summaries = Vec::new();
    for symbol in scheduler_symbols() {
        summaries.push(tick_symbol(state, tenant_id, &symbol, sentiment).await);
    }
    summaries.join(" | ")
}

/// One symbol of a tick: cycle on live closes + funding, then at most one
/// paper order. `sentiment` is the tick-wide RSS/Fear&Greed blend.
async fn tick_symbol(
    state: &AppState,
    tenant_id: uuid::Uuid,
    symbol: &str,
    sentiment: f32,
) -> String {
    let base = symbol.split('-').next().unwrap_or(symbol);
    let closes = live_closes_for(symbol).await.unwrap_or_default();
    let funding = fetch_funding(base).await;
    let ctx = MarketContext { sentiment, funding };
    let decisions = run_cycle_with_context(symbol, &closes, ctx);
    if let Err(e) = record_decisions(&state.pool, tenant_id, &decisions).await {
        return format!("{symbol} degraded: decisions not recorded: {e}");
    }
    let action_of = |id: &str| decisions.iter().find(|d| d.agent == id).map(|d| d.action.as_str());
    let (strategy, risk, execution) =
        (action_of("strategy"), action_of("risk"), action_of("execution"));
    if strategy != Some("buy") && strategy != Some("sell") {
        return format!("{symbol} hold: strategy={}", strategy.unwrap_or("?"));
    }
    if risk != Some("approve") || execution == Some("hold") {
        return format!(
            "{symbol} blocked: strategy={} risk={}",
            strategy.unwrap_or("?"),
            risk.unwrap_or("?")
        );
    }
    let side = strategy.unwrap_or("hold");
    let price = match closes.last().copied() {
        Some(p) if p.is_finite() && p > 0.0 => p,
        _ => return format!("{symbol} skipped: no price to trade"),
    };
    if side == "sell" {
        // No short selling on spot paper: cap the sell at what we hold.
        let (held, _) = long_held(&state.pool, tenant_id, symbol).await;
        let capped = crate::exits::cap_sell_qty(sized_qty(&closes, price), held);
        if capped <= 0.0 {
            return format!("{symbol} skipped: sell signal with no position held");
        }
        let slot = chrono::Utc::now().timestamp() / scheduler_interval_secs() as i64;
        let key = format!("sched-{side}-{symbol}-{slot}");
        match crate::orders::place_paper_order(
            &state.pool,
            tenant_id,
            "coinbase",
            symbol,
            side,
            capped,
            price,
            Some(&key),
        )
        .await
        {
            Ok(order) => {
                return format!(
                    "{symbol} placed {side} {} @ {:.2} (id {})",
                    order.qty, order.price, order.id
                )
            }
            Err((status, msg)) => return format!("{symbol} order rejected ({status}): {msg}"),
        }
    }
    if last_side(&state.pool, tenant_id, symbol).await.as_deref() == Some(side) {
        return format!("{symbol} skipped: already positioned {side}");
    }
    let qty = sized_qty(&closes, price);
    if qty <= 0.0 {
        return format!("{symbol} skipped: no size to trade");
    }
    let slot = chrono::Utc::now().timestamp() / scheduler_interval_secs() as i64;
    let key = format!("sched-{side}-{symbol}-{slot}");
    match crate::orders::place_paper_order(
        &state.pool,
        tenant_id,
        "coinbase",
        symbol,
        side,
        qty,
        price,
        Some(&key),
    )
    .await
    {
        Ok(order) => format!("{symbol} placed {side} {} @ {:.2} (id {})", order.qty, order.price, order.id),
        Err((status, msg)) => format!("{symbol} order rejected ({status}): {msg}"),
    }
}

/// Spawn the autonomous loop. Returns immediately; the loop logs one
/// line per tick and never brings the server down.
pub fn spawn(state: AppState) {
    tokio::spawn(async move {
        let every = scheduler_interval_secs();
        tracing::info!("scheduler on: {} every {every}s (paper only)", scheduler_symbols().join(","));
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(every)).await;
            let summary = tick(&state).await;
            tracing::info!("scheduler: {summary}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_defaults_and_clamps() {
        std::env::remove_var("SCHEDULER_INTERVAL_SECS");
        assert_eq!(scheduler_interval_secs(), 300);
        std::env::set_var("SCHEDULER_INTERVAL_SECS", "10");
        assert_eq!(scheduler_interval_secs(), 300, "below minimum clamps to default");
        std::env::set_var("SCHEDULER_INTERVAL_SECS", "60");
        assert_eq!(scheduler_interval_secs(), 60);
        std::env::remove_var("SCHEDULER_INTERVAL_SECS");
    }

    #[test]
    fn enabled_defaults_true() {
        std::env::remove_var("SCHEDULER_ENABLED");
        assert!(scheduler_enabled());
        std::env::set_var("SCHEDULER_ENABLED", "false");
        assert!(!scheduler_enabled());
        std::env::remove_var("SCHEDULER_ENABLED");
    }

    #[tokio::test]
    async fn tick_without_db_skips() {
        let state = AppState {
            pool: sqlx::PgPool::connect_lazy("postgres://127.0.0.1:1/none")
                .expect("test lazy pool"),
            jwt_secret: "test".to_string(),
        };
        let summary = tick(&state).await;
        assert!(
            summary.contains("skipped") || summary.contains("degraded"),
            "tick must degrade without DB, got {summary:?}"
        );
    }
}
