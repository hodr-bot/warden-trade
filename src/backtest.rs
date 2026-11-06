use serde::Serialize;

/// Deterministic backtest summary.
///
/// Pure value object: no I/O, no network, no secrets, no clock access.
/// `created_at` is a fixed deterministic marker to keep `run_backtest` pure.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BacktestResult {
    pub strategy: String,
    pub trades: usize,
    pub wins: usize,
    pub total_pnl_f64: f64,
    pub created_at: String,
}

/// Run a deterministic backtest over a price series.
///
/// - `strategy`: strategy name (copied into the result).
/// - `prices`: slice of prices in chronological order.
///
/// Rules (pure, deterministic):
/// - one trade per consecutive price pair, so `trades = prices.len() - 1`
///   (0 when fewer than 2 prices).
/// - an up move (`next > current`) counts as one win.
/// - `total_pnl_f64` is the sum of positive diffs only.
pub fn run_backtest(strategy: &str, prices: &[f64]) -> BacktestResult {
    if prices.len() < 2 {
        return BacktestResult {
            strategy: strategy.to_string(),
            trades: 0,
            wins: 0,
            total_pnl_f64: 0.0,
            created_at: "1970-01-01T00:00:00Z".to_string(),
        };
    }

    let mut wins: usize = 0;
    let mut total_pnl_f64: f64 = 0.0;

    for window in prices.windows(2) {
        let diff = window[1] - window[0];
        if diff > 0.0 {
            wins += 1;
            total_pnl_f64 += diff;
        }
    }

    BacktestResult {
        strategy: strategy.to_string(),
        trades: prices.len() - 1,
        wins,
        total_pnl_f64,
        created_at: "1970-01-01T00:00:00Z".to_string(),
    }
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
    }

    #[test]
    fn rising_prices_are_all_wins() {
        let prices = vec![100.0, 101.5, 103.0, 107.25];
        let result = run_backtest("sma", &prices);
        assert_eq!(result.trades, 3);
        assert_eq!(result.wins, 3);
        // (101.5-100.0) + (103.0-101.5) + (107.25-103.0) = 7.25
        assert!((result.total_pnl_f64 - 7.25).abs() < 1e-9);
    }
}
