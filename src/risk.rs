//! Risk limits and pre-trade order checks.
//!
//! Pure, deterministic logic only: no network, no secrets, no I/O.

/// Configured risk limits for pre-trade checks.
#[derive(Debug, Clone)]
pub struct RiskLimits {
    /// Maximum allowed notional (qty * price) for a single order.
    pub max_order_notional: f64,
    /// Maximum allowed exposure for a single asset (stub check for now).
    pub max_exposure_per_asset: f64,
    /// Maximum allowed daily loss (stub check for now).
    pub max_daily_loss: f64,
    /// Maximum number of simultaneously open orders (stub check for now).
    pub max_open_orders: u32,
    /// When true, all orders are rejected.
    pub kill_switch: bool,
}

/// Intent to place a single order.
#[derive(Debug, Clone)]
pub struct OrderIntent {
    /// Trading symbol, e.g. "BTC-USD".
    pub symbol: String,
    /// Order side, e.g. "buy" or "sell".
    pub side: String,
    /// Order quantity in base currency. Must be > 0.
    pub qty: f64,
    /// Limit price in quote currency.
    pub price: f64,
}

/// Pure pre-trade check: returns `Ok(())` if the order may proceed,
/// or `Err(String)` with the rejection reason.
pub fn check_order(limits: &RiskLimits, intent: &OrderIntent) -> Result<(), String> {
    if limits.kill_switch {
        return Err("kill_switch engaged: all orders blocked".to_string());
    }
    if !intent.qty.is_finite() || intent.qty <= 0.0 {
        return Err("qty must be positive".to_string());
    }
    if !intent.price.is_finite() || intent.price <= 0.0 {
        return Err("price must be positive".to_string());
    }
    let notional = intent.qty * intent.price;
    if !notional.is_finite() {
        return Err("notional is not finite".to_string());
    }
    if notional > limits.max_order_notional {
        return Err(format!(
            "order notional {notional} exceeds max_order_notional {}",
            limits.max_order_notional
        ));
    }
    // Stub: per-asset exposure check — currently always passes.
    // Future work: compare current exposure + notional against
    // limits.max_exposure_per_asset using real positions.
    let _ = limits.max_exposure_per_asset;
    let _ = limits.max_daily_loss;
    let _ = limits.max_open_orders;
    Ok(())
}

// Full position-aware checks live in `check_order_full` below: they need
// the database (open positions, exposure, today's realized PnL), so
// they cannot run in this pure function. `place_paper_order` always
// calls the full gate; this one stays the hard synchronous floor.

/// Tiny epsilon under which a net quantity counts as flat.
///
/// Float dust from partial fills must not pin a "position" open forever.
const FLAT_EPS: f64 = 1e-9;

/// FIFO realized PnL restricted to closes on/after `day_start_ts`.
///
/// Pure: `fills` is `(side, qty, price, created_at_epoch)` oldest first.
/// Sells before the threshold still consume lots (correct cost basis) but
/// their PnL is not counted; sells on/after it count at match time. Buys
/// never realize anything by themselves.
pub fn fifo_realized_since(fills: &[(String, f64, f64, i64)], day_start_ts: i64) -> f64 {
    let mut lots: Vec<(f64, f64)> = Vec::new();
    let mut realized = 0.0;
    for (side, qty, price, ts) in fills {
        if side == "buy" {
            lots.push((*qty, *price));
        } else {
            let mut rest = *qty;
            while rest > 0.0 {
                match lots.first_mut() {
                    Some((q, p)) => {
                        let m = rest.min(*q);
                        if *ts >= day_start_ts {
                            realized += (*price - *p) * m;
                        }
                        *q -= m;
                        rest -= m;
                        if *q <= 0.0 {
                            lots.remove(0);
                        }
                    }
                    None => break,
                }
            }
        }
    }
    realized
}

/// Net held quantity of `symbol` for `tenant_id` (buys minus sells).
///
/// Read-only. Unreadable database degrades to `0.0` with a warning: the
/// hard notional/kill-switch floor in [`check_order`] still applies, so a
/// blind DB never means unlimited orders.
pub async fn net_qty_for(pool: &sqlx::PgPool, tenant_id: uuid::Uuid, symbol: &str) -> f64 {
    let row = sqlx::query(
        "SELECT COALESCE(SUM(CASE WHEN side = 'buy' THEN qty ELSE -qty END), 0.0) AS net
         FROM paper_orders WHERE tenant_id = $1 AND symbol = $2",
    )
    .bind(tenant_id)
    .bind(symbol)
    .fetch_optional(pool)
    .await;
    match row {
        Ok(Some(r)) => {
            use sqlx::Row;
            r.try_get("net").unwrap_or(0.0)
        }
        Ok(None) => 0.0,
        Err(e) => {
            tracing::warn!("net_qty_for unreadable, assuming flat: {e}");
            0.0
        }
    }
}

/// Count of symbols with a non-flat net position for `tenant_id`.
///
/// Read-only. Unreadable database degrades to `0` with a warning (same
/// rationale as [`net_qty_for`]).
pub async fn open_position_count(pool: &sqlx::PgPool, tenant_id: uuid::Uuid) -> u32 {
    let rows = sqlx::query(
        "SELECT COALESCE(SUM(CASE WHEN side = 'buy' THEN qty ELSE -qty END), 0.0) AS net
         FROM paper_orders WHERE tenant_id = $1 GROUP BY symbol",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await;
    match rows {
        Ok(rows) => {
            use sqlx::Row;
            rows.iter()
                .filter(|r| r.try_get::<f64, _>("net").unwrap_or(0.0).abs() > FLAT_EPS)
                .count() as u32
        }
        Err(e) => {
            tracing::warn!("open_position_count unreadable, assuming none: {e}");
            0
        }
    }
}

/// Realized PnL since `day_start_ts` (epoch seconds) across all symbols.
///
/// FIFO per symbol: earlier sells still consume lots for a correct cost
/// basis, only closes on/after the threshold count. Read-only; unreadable
/// database degrades to `0.0` with a warning (same rationale as
/// [`net_qty_for`]).
pub async fn realized_since(pool: &sqlx::PgPool, tenant_id: uuid::Uuid, day_start_ts: i64) -> f64 {
    let rows = sqlx::query(
        "SELECT symbol, side, qty, price, extract(epoch FROM created_at)::BIGINT AS ts
         FROM paper_orders WHERE tenant_id = $1 ORDER BY created_at ASC",
    )
    .bind(tenant_id)
    .fetch_all(pool)
    .await;
    let rows = match rows {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("realized_since unreadable, assuming zero: {e}");
            return 0.0;
        }
    };
    use sqlx::Row;
    use std::collections::BTreeMap;
    let mut by_symbol: BTreeMap<String, Vec<(String, f64, f64, i64)>> = BTreeMap::new();
    for r in &rows {
        let symbol: String = r.try_get("symbol").unwrap_or_default();
        if symbol.is_empty() {
            continue;
        }
        by_symbol.entry(symbol).or_default().push((
            r.try_get("side").unwrap_or_else(|_| "buy".to_string()),
            r.try_get("qty").unwrap_or(0.0),
            r.try_get("price").unwrap_or(0.0),
            r.try_get("ts").unwrap_or(0),
        ));
    }
    by_symbol
        .values()
        .map(|fills| fifo_realized_since(fills, day_start_ts))
        .sum()
}

/// Start of the UTC day containing `now_ts`, as epoch seconds
/// (daily-loss window). Pure in `now_ts` so tests pin exact midnights.
pub fn utc_day_start_ts(now_ts: i64) -> i64 {
    now_ts - now_ts.rem_euclid(86_400)
}

/// Full pre-trade gate: hard synchronous floor plus position-aware limits.
///
/// Runs [`check_order`] first (kill switch, positive qty/price, single
/// order notional — always enforced, even when the database is blind),
/// then, for opening/augmenting orders (`buy`):
/// - too many distinct open positions → reject;
/// - resulting exposure (`held + qty` at intent price) over
///   `max_exposure_per_asset` → reject;
/// - today's realized PnL worse than `-max_daily_loss` → reject (entries
///   only: exits/sells always stay allowed so a losing day can be cut).
///
/// `sell` intents only face the hard floor: never block an exit.
pub async fn check_order_full(
    pool: &sqlx::PgPool,
    tenant_id: uuid::Uuid,
    limits: &RiskLimits,
    intent: &OrderIntent,
) -> Result<(), String> {
    check_order(limits, intent)?;
    if intent.side != "buy" {
        return Ok(());
    }
    let held = net_qty_for(pool, tenant_id, &intent.symbol).await;
    if held.abs() <= FLAT_EPS {
        let open = open_position_count(pool, tenant_id).await;
        if open >= limits.max_open_orders {
            return Err(format!(
                "too many open positions ({open} >= {})",
                limits.max_open_orders
            ));
        }
    }
    let exposure = (held + intent.qty) * intent.price;
    if exposure.is_finite() && exposure > limits.max_exposure_per_asset {
        return Err(format!(
            "exposure {exposure:.2} exceeds max_exposure_per_asset {}",
            limits.max_exposure_per_asset
        ));
    }
    let today = realized_since(pool, tenant_id, utc_day_start_ts(chrono::Utc::now().timestamp())).await;
    if today < -limits.max_daily_loss {
        return Err(format!(
            "daily loss {today:.2} exceeds max_daily_loss {}: sells only",
            limits.max_daily_loss
        ));
    }
    Ok(())
}

impl RiskLimits {
    /// Conservative fallback used when the database is unreachable.
    ///
    /// Pure constructor: no network, no secrets, no I/O. The server stays
    /// up in degraded mode instead of accepting unlimited orders.
    pub fn conservative_default() -> Self {
        Self {
            max_order_notional: 10_000.0,
            max_exposure_per_asset: 50_000.0,
            max_daily_loss: 1_000.0,
            max_open_orders: 10,
            kill_switch: false,
        }
    }
}

/// Load the risk limits configured for `tenant_id`.
///
/// Read-only: the risk engine never writes limits. When the row is missing
/// or the database is unreachable, returns [`RiskLimits::conservative_default`]
/// and logs why, so order flow degrades closed (tight limits) rather than
/// open. Agents have no path to this table's writes: only a direct DB
/// operator can change limits.
pub async fn load_limits(pool: &sqlx::PgPool, tenant_id: uuid::Uuid) -> RiskLimits {
    let row = sqlx::query(
        "SELECT max_order_notional, max_exposure_per_asset, max_daily_loss, max_open_orders, kill_switch
         FROM risk_limits WHERE tenant_id = $1",
    )
    .bind(tenant_id)
    .fetch_optional(pool)
    .await;
    match row {
        Ok(Some(row)) => {
            use sqlx::Row;
            RiskLimits {
                max_order_notional: row.try_get("max_order_notional").unwrap_or(10_000.0),
                max_exposure_per_asset: row.try_get("max_exposure_per_asset").unwrap_or(50_000.0),
                max_daily_loss: row.try_get("max_daily_loss").unwrap_or(1_000.0),
                max_open_orders: row.try_get::<i32, _>("max_open_orders").unwrap_or(10).max(1) as u32,
                kill_switch: row.try_get("kill_switch").unwrap_or(false),
            }
        }
        Ok(None) => {
            tracing::warn!("no risk_limits row, using conservative defaults");
            RiskLimits::conservative_default()
        }
        Err(e) => {
            tracing::warn!("risk_limits unreadable, using conservative defaults: {e}");
            RiskLimits::conservative_default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_limits() -> RiskLimits {
        RiskLimits {
            max_order_notional: 10_000.0,
            max_exposure_per_asset: 50_000.0,
            max_daily_loss: 1_000.0,
            max_open_orders: 10,
            kill_switch: false,
        }
    }

    fn test_intent(qty: f64, price: f64) -> OrderIntent {
        OrderIntent {
            symbol: "BTC-USD".to_string(),
            side: "buy".to_string(),
            qty,
            price,
        }
    }

    #[test]
    fn kill_switch_blocks_all_orders() {
        let mut limits = test_limits();
        limits.kill_switch = true;
        let intent = test_intent(1.0, 100.0);
        let result = check_order(&limits, &intent);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("kill_switch"));
    }

    #[test]
    fn oversize_notional_is_blocked() {
        let limits = test_limits();
        // notional = 100 * 500 = 50_000 > 10_000 max
        let intent = test_intent(100.0, 500.0);
        let result = check_order(&limits, &intent);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds max_order_notional"));
    }

    #[test]
    fn valid_order_passes() {
        let limits = test_limits();
        // notional = 1 * 100 = 100 <= 10_000 max
        let intent = test_intent(1.0, 100.0);
        assert!(check_order(&limits, &intent).is_ok());
    }

    #[test]
    fn utc_day_start_is_midnight() {
        // 2026-11-08 00:00:00 UTC == 1794096000.
        assert_eq!(utc_day_start_ts(1794096000), 1794096000);
        // One second before midnight still maps to the previous day.
        assert_eq!(utc_day_start_ts(1794095999), 1794009600);
        // Noon maps back to midnight of the same day.
        assert_eq!(utc_day_start_ts(1794139200), 1794096000);
    }

    #[test]
    fn realized_since_counts_only_today_sells() {
        // Day D starts at ts 1_000. A buy yesterday (900) matched by a
        // sell today (1100) counts: +100. A sell yesterday (950) matching
        // an older buy (800) does not count.
        let fills = vec![
            ("buy".to_string(), 1.0, 90.0, 800),
            ("sell".to_string(), 1.0, 95.0, 950),
            ("buy".to_string(), 1.0, 100.0, 900),
            ("sell".to_string(), 1.0, 200.0, 1100),
        ];
        assert!((fifo_realized_since(&fills, 1000) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn realized_since_empty_when_no_sell_today() {
        let fills = vec![
            ("buy".to_string(), 1.0, 100.0, 900),
            ("sell".to_string(), 1.0, 150.0, 950),
        ];
        assert!((fifo_realized_since(&fills, 1000)).abs() < 1e-9);
    }

    #[test]
    fn realized_since_ignores_unmatched_sells() {
        // Sell with no prior buy: no cost basis, contributes nothing
        // (same rule as the dashboard FIFO), even when dated today.
        let fills = vec![("sell".to_string(), 1.0, 150.0, 1100)];
        assert!((fifo_realized_since(&fills, 1000)).abs() < 1e-9);
    }

    // --- Database-backed gate proof -------------------------------------
    // Skipped (not failed) when `WARDEN_TEST_DATABASE_URL` is unset, so the
    // default `cargo test` stays hermetic. Point it at a scratch Postgres
    // with migrations applied:
    //   WARDEN_TEST_DATABASE_URL=postgres://.../db cargo test --offline risk::
    // Each scenario gets its own tenant: no cross-talk, no cleanup hazards.

    async fn proof_pool() -> Option<sqlx::PgPool> {
        match std::env::var("WARDEN_TEST_DATABASE_URL") {
            Ok(url) => Some(sqlx::PgPool::connect(&url).await.expect("proof DB")),
            Err(_) => {
                eprintln!("skipped: WARDEN_TEST_DATABASE_URL unset");
                None
            }
        }
    }

    async fn proof_tenant(pool: &sqlx::PgPool, col: &str, val: &str) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, 'risk-proof')")
            .bind(id)
            .execute(pool)
            .await
            .expect("insert tenant");
        // `col`/`val` are hardcoded in-test literals, never user input.
        sqlx::query(&format!(
            "INSERT INTO risk_limits (tenant_id, {col}) VALUES ($1, {val})"
        ))
        .bind(id)
        .execute(pool)
        .await
        .expect("insert risk_limits");
        id
    }

    async fn proof_fill(
        pool: &sqlx::PgPool,
        tenant: uuid::Uuid,
        symbol: &str,
        side: &str,
        qty: f64,
        price: f64,
    ) {
        sqlx::query(
            "INSERT INTO paper_orders (tenant_id, exchange, symbol, side, qty, price, status)
             VALUES ($1, 'coinbase', $2, $3, $4, $5, 'filled')",
        )
        .bind(tenant)
        .bind(symbol)
        .bind(side)
        .bind(qty)
        .bind(price)
        .execute(pool)
        .await
        .expect("insert fill");
    }

    fn proof_intent(symbol: &str, side: &str, qty: f64, price: f64) -> OrderIntent {
        OrderIntent {
            symbol: symbol.to_string(),
            side: side.to_string(),
            qty,
            price,
        }
    }

    fn wide_limits() -> RiskLimits {
        RiskLimits {
            max_order_notional: 1_000_000.0,
            max_exposure_per_asset: 1_000_000.0,
            max_daily_loss: 1_000_000.0,
            max_open_orders: 100,
            kill_switch: false,
        }
    }

    #[tokio::test]
    async fn db_exposure_gate_blocks_overweight_buy() {
        let Some(pool) = proof_pool().await else { return };
        let t = proof_tenant(&pool, "max_exposure_per_asset", "1000").await;
        // Exposure is 700 after this fill.
        proof_fill(&pool, t, "BTC-USD", "buy", 0.01, 70_000.0).await;

        let mut lim = wide_limits();
        lim.max_exposure_per_asset = 1000.0;
        // 700 + 700 = 1400 > 1000 → blocked.
        let err = check_order_full(&pool, t, &lim, &proof_intent("BTC-USD", "buy", 0.01, 70_000.0))
            .await
            .expect_err("overweight buy must be rejected");
        assert!(err.contains("exposure"), "unexpected: {err}");
        // 700 + 280 = 980 <= 1000 → allowed.
        check_order_full(&pool, t, &lim, &proof_intent("BTC-USD", "buy", 0.004, 70_000.0))
            .await
            .expect("light buy within exposure must pass");
    }

    #[tokio::test]
    async fn db_open_positions_gate_blocks_new_symbol() {
        let Some(pool) = proof_pool().await else { return };
        let t = proof_tenant(&pool, "max_open_orders", "1").await;
        proof_fill(&pool, t, "BTC-USD", "buy", 0.01, 70_000.0).await;

        let mut lim = wide_limits();
        lim.max_open_orders = 1;
        // Second symbol while one position is open → blocked.
        let err = check_order_full(&pool, t, &lim, &proof_intent("ETH-USD", "buy", 1.0, 2000.0))
            .await
            .expect_err("second position must be rejected");
        assert!(err.contains("open positions"), "unexpected: {err}");
        // Same symbol (augmenting, exposure wide open) → allowed.
        check_order_full(&pool, t, &lim, &proof_intent("BTC-USD", "buy", 0.001, 70_000.0))
            .await
            .expect("augmenting the open position must pass");
    }

    #[tokio::test]
    async fn db_daily_loss_gate_blocks_buys_but_not_sells() {
        let Some(pool) = proof_pool().await else { return };
        let t = proof_tenant(&pool, "max_daily_loss", "10").await;
        // Realized today: buy 1 @ 100, sell 1 @ 50 → -50, beyond the 10 limit.
        proof_fill(&pool, t, "BTC-USD", "buy", 1.0, 100.0).await;
        proof_fill(&pool, t, "BTC-USD", "sell", 1.0, 50.0).await;

        let mut lim = wide_limits();
        lim.max_daily_loss = 10.0;
        let err = check_order_full(&pool, t, &lim, &proof_intent("BTC-USD", "buy", 0.1, 60.0))
            .await
            .expect_err("buy after daily loss must be rejected");
        assert!(err.contains("daily loss"), "unexpected: {err}");
        // Exits stay open: never trap a position because the day went red.
        check_order_full(&pool, t, &lim, &proof_intent("BTC-USD", "sell", 0.1, 60.0))
            .await
            .expect("sells must stay allowed after daily loss");
    }
}
