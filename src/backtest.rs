use serde::Serialize;

/// Deterministic backtest summary for a moving-average crossover strategy.
///
/// Pure value object: no I/O, no network, no secrets, no clock access.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BacktestResult {
    pub strategy: String,
    pub trades: usize,
    pub wins: usize,
    /// Net PnL in price units per 1.0 unit traded, after fees.
    pub total_pnl_f64: f64,
    /// Largest peak-to-trough drop of the equity curve, in price units.
    pub max_drawdown_f64: f64,
    /// `wins / trades`, 0 when no trade completed.
    pub win_rate_f64: f64,
}

/// Run a deterministic SMA-crossover backtest over a price series.
///
/// - `strategy`: strategy name (copied into the result, unused by logic).
/// - `prices`: slice of prices in chronological order.
///
/// Rules (pure, deterministic, long-or-flat, 1.0 unit per trade):
/// - fast SMA(3) vs slow SMA(8); enter long when fast crosses above slow,
///   exit when fast crosses back below (or at the last price).
/// - each round trip pays [`crate::fees::FEE_BPS`] basis points per side
///   on notional (J3: same const the live fills pay).
/// - `total_pnl_f64` is net realized PnL; `wins` counts profitable trips.
/// - `max_drawdown_f64` is the worst peak-to-trough equity drop.
/// - fewer than 9 prices yields zero trades (slow window never fills).
pub fn run_backtest(strategy: &str, prices: &[f64]) -> BacktestResult {
    const FAST: usize = 3;
    const SLOW: usize = 8;
    let empty = || BacktestResult {
        strategy: strategy.to_string(),
        trades: 0,
        wins: 0,
        total_pnl_f64: 0.0,
        max_drawdown_f64: 0.0,
        win_rate_f64: 0.0,
    };
    if prices.len() <= SLOW {
        return empty();
    }

    let sma = |end: usize, window: usize| -> f64 {
        prices[end + 1 - window..=end].iter().sum::<f64>() / window as f64
    };

    let mut trades = 0usize;
    let mut wins = 0usize;
    let mut total_pnl = 0.0f64;
    let mut equity = 0.0f64;
    let mut peak = 0.0f64;
    let mut max_dd = 0.0f64;
    let mut entry: Option<f64> = None;

    let mut track = |eq: f64, peak: &mut f64, max_dd: &mut f64| {
        if eq > *peak {
            *peak = eq;
        }
        let dd = *peak - eq;
        if dd > *max_dd {
            *max_dd = dd;
        }
    };

    for i in SLOW..prices.len() {
        let fast_now = sma(i, FAST);
        let slow_now = sma(i, SLOW);
        let fast_prev = sma(i - 1, FAST);
        let slow_prev = sma(i - 1, SLOW);
        if entry.is_none() && fast_prev <= slow_prev && fast_now > slow_now {
            entry = Some(prices[i]);
        } else if entry.is_some() && fast_prev >= slow_prev && fast_now < slow_now {
            let open = entry.take().unwrap();
            let gross = prices[i] - open;
            let fee = (open + prices[i]) * crate::fees::FEE_BPS / 10_000.0;
            let net = gross - fee;
            trades += 1;
            if net > 0.0 {
                wins += 1;
            }
            equity += net;
            total_pnl += net;
            track(equity, &mut peak, &mut max_dd);
        }
    }
    if let Some(open) = entry.take() {
        let close = prices[prices.len() - 1];
        let gross = close - open;
        let fee = (open + close) * crate::fees::FEE_BPS / 10_000.0;
        let net = gross - fee;
        trades += 1;
        if net > 0.0 {
            wins += 1;
        }
        equity += net;
        total_pnl += net;
        track(equity, &mut peak, &mut max_dd);
    }

    BacktestResult {
        strategy: strategy.to_string(),
        trades,
        wins,
        total_pnl_f64: total_pnl,
        max_drawdown_f64: max_dd,
        win_rate_f64: if trades > 0 {
            wins as f64 / trades as f64
        } else {
            0.0
        },
    }
}

/// Backtest any [`crate::strategies::Strategy`] over a price series (J3).
///
/// Same economics as [`run_backtest`] (long-or-flat, 1.0 unit, fee per
/// side via [`crate::fees`]) but the entry/exit rule is the strategy's
/// OWN `signal`, evaluated per bar on the closes seen so far — the exact
/// function the live loop votes with. No duplicated crossover logic.
pub fn run_backtest_strategy(
    strategy: &dyn crate::strategies::Strategy,
    prices: &[f64],
) -> BacktestResult {
    use crate::strategies::Signal;
    let empty = || BacktestResult {
        strategy: strategy.name().to_string(),
        trades: 0,
        wins: 0,
        total_pnl_f64: 0.0,
        max_drawdown_f64: 0.0,
        win_rate_f64: 0.0,
    };
    if prices.len() <= 8 {
        return empty();
    }
    let mut trades = 0usize;
    let mut wins = 0usize;
    let mut total_pnl = 0.0f64;
    let mut equity = 0.0f64;
    let mut peak = 0.0f64;
    let mut max_dd = 0.0f64;
    let mut entry: Option<f64> = None;
    let settle = |equity: f64, peak: &mut f64, max_dd: &mut f64| {
        if equity > *peak {
            *peak = equity;
        }
        let dd = *peak - equity;
        if dd > *max_dd {
            *max_dd = dd;
        }
    };
    for i in 0..prices.len() {
        let vote = strategy.signal(&prices[..=i]).signal;
        if entry.is_none() && vote == Signal::Buy {
            entry = Some(prices[i]);
        } else if let Some(open) = entry.take() {
            if vote == Signal::Sell {
                let gross = prices[i] - open;
                let fee = (open + prices[i]) * crate::fees::FEE_BPS / 10_000.0;
                let net = gross - fee;
                trades += 1;
                if net > 0.0 {
                    wins += 1;
                }
                equity += net;
                total_pnl += net;
                settle(equity, &mut peak, &mut max_dd);
                // entry consumed by the take() above
            } else {
                entry = Some(open);
            }
        }
    }
    if let Some(open) = entry.take() {
        let close = prices[prices.len() - 1];
        let gross = close - open;
        let fee = (open + close) * crate::fees::FEE_BPS / 10_000.0;
        let net = gross - fee;
        trades += 1;
        if net > 0.0 {
            wins += 1;
        }
        equity += net;
        total_pnl += net;
        settle(equity, &mut peak, &mut max_dd);
    }
    BacktestResult {
        strategy: strategy.name().to_string(),
        trades,
        wins,
        total_pnl_f64: total_pnl,
        max_drawdown_f64: max_dd,
        win_rate_f64: if trades > 0 {
            wins as f64 / trades as f64
        } else {
            0.0
        },
    }
}

/// Query params for the live backtest endpoint.
#[derive(Debug, serde::Deserialize)]
pub struct BacktestQuery {
    /// Product id, e.g. `"BTC-USD"`. Defaults to `"BTC-USD"`.
    pub product: Option<String>,
    /// Candle granularity in seconds (60/300/900/3600/21600/86400).
    /// Defaults to `86400` (daily, ~300 days of history).
    pub granularity: Option<u64>,
}

/// Live backtest report: real Exchange candles through the SMA engine.
#[derive(Debug, serde::Serialize)]
pub struct LiveBacktestReport {
    pub product: String,
    pub granularity: u64,
    pub candles: usize,
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub result: BacktestResult,
    pub generated_at: String,
}

/// `GET /api/backtest`: fetch up to 300 real Coinbase Exchange candles
/// (login required, read-only) and run the SMA-crossover engine on the
/// closes. Degrades to a zero-trade report when the feed is unreachable.
pub async fn backtest_handler(
    _tenant: crate::state::Tenant,
    axum::extract::Query(query): axum::extract::Query<BacktestQuery>,
) -> axum::Json<LiveBacktestReport> {
    let product = query.product.unwrap_or_else(|| "BTC-USD".to_string());
    let granularity = query.granularity.unwrap_or(86400);
    let client = crate::coinbase::CoinbaseClient::new();
    let (candles, from, to, result) = match client.fetch_candles(&product, granularity).await {
        Ok(candles) => {
            let closes: Vec<f64> = candles.iter().map(|c| c.close).collect();
            let from = candles.first().map(|c| c.time);
            let to = candles.last().map(|c| c.time);
            // J3 parity: the endpoint runs the live SMA rule itself,
            // not a parallel re-implementation.
            let result =
                run_backtest_strategy(&crate::strategies::SmaCross, &closes);
            (candles.len(), from, to, result)
        }
        Err(e) => {
            tracing::warn!("backtest feed degraded: {e}");
            (
                0,
                None,
                None,
                BacktestResult {
                    strategy: "sma-3-8".to_string(),
                    trades: 0,
                    wins: 0,
                    total_pnl_f64: 0.0,
                    max_drawdown_f64: 0.0,
                    win_rate_f64: 0.0,
                },
            )
        }
    };
    axum::Json(LiveBacktestReport {
        product,
        granularity,
        candles,
        from,
        to,
        result,
        generated_at: chrono::Utc::now().to_rfc3339(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_prices_yields_zero_trades() {
        let result = run_backtest("sma", &[]);
        assert_eq!(result.strategy, "sma");
        assert_eq!(result.trades, 0);
        assert_eq!(result.wins, 0);
        assert_eq!(result.total_pnl_f64, 0.0);
        assert_eq!(result.max_drawdown_f64, 0.0);
        assert_eq!(result.win_rate_f64, 0.0);
    }

    #[test]
    fn short_series_below_slow_window_yields_zero_trades() {
        let prices = vec![100.0, 101.0, 102.0, 103.0];
        let result = run_backtest("sma", &prices);
        assert_eq!(result.trades, 0);
        assert_eq!(result.win_rate_f64, 0.0);
    }

    #[test]
    fn steady_climb_enters_once_and_profits() {
        // Flat then rising: crossover fires once, force-closed at the top.
        let prices = vec![
            100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, //
            101.0, 102.0, 103.0, 104.0, 105.0,
        ];
        let result = run_backtest("sma", &prices);
        assert_eq!(result.trades, 1);
        assert_eq!(result.wins, 1);
        assert!(result.total_pnl_f64 > 0.0, "pnl = {}", result.total_pnl_f64);
        assert_eq!(result.win_rate_f64, 1.0);
    }

    #[test]
    fn steady_decline_never_enters() {
        let prices: Vec<f64> = (0..15).map(|i| 200.0 - i as f64).collect();
        let result = run_backtest("sma", &prices);
        assert_eq!(result.trades, 0);
        assert_eq!(result.total_pnl_f64, 0.0);
    }

    #[test]
    fn strategy_engine_matches_legacy_sma_engine() {
        // J3 parity: the shared-rule engine must agree trade-for-trade
        // with the legacy inline SMA engine on every shape of series.
        use crate::strategies::SmaCross;
        let series: Vec<Vec<f64>> = vec![
            vec![],
            vec![100.0, 101.0],
            vec![100.0; 15],
            vec![
                100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, //
                101.0, 102.0, 103.0, 104.0, 105.0,
            ],
            (0..15).map(|i| 200.0 - i as f64).collect(),
            vec![
                100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, //
                110.0, 120.0, 90.0, 80.0, 70.0, 75.0, 85.0, 95.0, 90.0, 80.0,
            ],
        ];
        for prices in &series {
            let legacy = run_backtest("sma-3-8", prices);
            let shared = run_backtest_strategy(&SmaCross, prices);
            assert_eq!(legacy.trades, shared.trades, "trades on {prices:?}");
            assert_eq!(legacy.wins, shared.wins, "wins on {prices:?}");
            assert!(
                (legacy.total_pnl_f64 - shared.total_pnl_f64).abs() < 1e-9,
                "pnl on {prices:?}"
            );
            assert!(
                (legacy.max_drawdown_f64 - shared.max_drawdown_f64).abs() < 1e-9,
                "dd on {prices:?}"
            );
        }
    }

    #[test]
    fn pump_then_dump_reports_drawdown() {
        // Flat, pump, then crash below entry: losing trade + drawdown > 0.
        let prices = vec![
            100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, //
            110.0, 120.0, 90.0, 80.0, 70.0,
        ];
        let result = run_backtest("sma", &prices);
        assert!(result.trades >= 1, "trades = {}", result.trades);
        assert!(result.total_pnl_f64 < 0.0, "pnl = {}", result.total_pnl_f64);
        assert!(result.max_drawdown_f64 > 0.0, "dd = {}", result.max_drawdown_f64);
    }
}
