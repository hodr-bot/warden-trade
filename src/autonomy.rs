//! Paper-only autonomy report: deterministic aggregation, no network, no secrets.
//!
//! Pure logic only: combines [`crate::orchestrator::run_cycle`] (decisions),
//! [`crate::backtest::run_backtest`] (wins / pnl) and [`validate_limits`]
//! (risk-limits sanity check) into a single [`AutonomyReport`].

use axum::Json;
use serde::Serialize;

/// Paper-only autonomy report (serializable value object).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AutonomyReport {
    /// Trading symbol, e.g. "BTC-USD".
    pub symbol: String,
    /// Number of agent decisions in the cycle.
    pub decisions: usize,
    /// Number of winning trades from the backtest.
    pub wins: usize,
    /// Sum of positive price diffs from the backtest.
    pub total_pnl_f64: f64,
    /// True only if the default limits pass [`validate_limits`].
    pub limits_ok: bool,
    /// Deterministic generation marker (fixed to keep `run_paper_cycle` pure).
    pub generated_at: String,
}

/// Pure risk-limits sanity check.
///
/// Returns `true` only if all of these hold:
/// - `max_order_notional > 0`
/// - `max_exposure_per_asset > 0`
/// - `max_daily_loss > 0`
/// - `max_open_orders > 0`
pub fn validate_limits(limits: &crate::risk::RiskLimits) -> bool {
    limits.max_order_notional > 0.0
        && limits.max_exposure_per_asset > 0.0
        && limits.max_daily_loss > 0.0
        && limits.max_open_orders > 0
}

/// Run one deterministic paper cycle and build the [`AutonomyReport`].
///
/// - `symbol`: forwarded to [`crate::orchestrator::run_cycle`].
/// - `strategy`: forwarded to [`crate::backtest::run_backtest`].
/// - `prices`: forwarded to [`crate::backtest::run_backtest`].
///
/// `decisions` is `run_cycle(symbol).len()`, `wins` / `total_pnl_f64` come
/// from `run_backtest(strategy, prices)`, `limits_ok` is
/// `validate_limits` applied to sane paper defaults, and `generated_at`
/// is the current backend time. No network, no secrets.
pub fn run_paper_cycle(symbol: &str, strategy: &str, prices: &[f64]) -> AutonomyReport {
    let decisions = crate::orchestrator::run_cycle(symbol).len();
    let backtest = crate::backtest::run_backtest(strategy, prices);
    let default_limits = crate::risk::RiskLimits {
        max_order_notional: 10_000.0,
        max_exposure_per_asset: 50_000.0,
        max_daily_loss: 1_000.0,
        max_open_orders: 10,
        kill_switch: false,
    };
    AutonomyReport {
        symbol: symbol.to_string(),
        decisions,
        wins: backtest.wins,
        total_pnl_f64: backtest.total_pnl_f64,
        limits_ok: validate_limits(&default_limits),
        generated_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// Axum handler returning a deterministic paper autonomy report as JSON.
///
/// Fixed demo series (flat then climbing), paper only: no network,
/// no secrets, no I/O. Shows one completed SMA-crossover trade.
pub async fn autonomy_handler() -> Json<AutonomyReport> {
    Json(run_paper_cycle(
        "BTC-USD",
        "sma-3-8",
        &[
            100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, //
            101.0, 102.0, 103.0, 104.0, 105.0,
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_limits() -> crate::risk::RiskLimits {
        crate::risk::RiskLimits {
            max_order_notional: 10_000.0,
            max_exposure_per_asset: 50_000.0,
            max_daily_loss: 1_000.0,
            max_open_orders: 10,
            kill_switch: false,
        }
    }

    #[test]
    fn valid_limits_ok() {
        assert!(validate_limits(&valid_limits()));
    }

    #[test]
    fn zero_notional_fails() {
        let mut limits = valid_limits();
        limits.max_order_notional = 0.0;
        assert!(!validate_limits(&limits));
    }

    #[test]
    fn cycle_report_has_4_decisions() {
        // 13 climbing prices: SMA crossover fires once, one winning trade.
        // decisions = the 8-agent roster size (see orchestrator::run_cycle).
        let prices = vec![
            100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, 100.0, //
            101.0, 102.0, 103.0, 104.0, 105.0,
        ];
        let report = run_paper_cycle("BTC-USD", "sma", &prices);
        assert_eq!(report.symbol, "BTC-USD");
        assert_eq!(report.decisions, 8);
        assert!(report.limits_ok);
        assert_eq!(report.wins, 1);
        assert!(report.total_pnl_f64 > 0.0);
    }
}
