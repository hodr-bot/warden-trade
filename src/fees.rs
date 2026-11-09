//! Execution costs, shared by live paper fills and the backtester (J3 parity).
//!
//! The dashboard used to hardcode fees at `"0"` while the backtest charged
//! 10 bps per side: paper P&L and backtested P&L disagreed by construction.
//! Both paths now use these constants, so a strategy that looks profitable
//! in backtest pays the same costs live.
//!
//! Pure math: no I/O, no network, no secrets.

/// Fee per side, in basis points (0.10% — typical CEX taker tier).
pub const FEE_BPS: f64 = 10.0;

/// Adverse slippage applied to market fills, in basis points (0.05%).
/// Buys fill above the mark, sells below: paper stays pessimistic.
pub const SLIPPAGE_BPS: f64 = 5.0;

/// Fee owed on a `notional` fill (one side).
pub fn fee_for(notional: f64) -> f64 {
    if !notional.is_finite() || notional <= 0.0 {
        return 0.0;
    }
    notional * FEE_BPS / 10_000.0
}

/// Execution price of a market fill against `mark`: adverse slippage.
/// `side` is `"buy"` or `"sell"` (anything else fills at the mark).
pub fn fill_price(side: &str, mark: f64) -> f64 {
    if !mark.is_finite() || mark <= 0.0 {
        return mark;
    }
    let slip = mark * SLIPPAGE_BPS / 10_000.0;
    match side {
        "buy" => mark + slip,
        "sell" => mark - slip,
        _ => mark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fee_matches_legacy_backtest_math() {
        // (open + close) * 10 / 10_000, exactly the old inline formula.
        let open = 100.0;
        let close = 110.0;
        assert!(((open + close) * FEE_BPS / 10_000.0 - fee_for(open) - fee_for(close)).abs() < 1e-9);
    }

    #[test]
    fn fee_is_ten_bps_per_side() {
        assert!((fee_for(10_000.0) - 10.0).abs() < 1e-9);
        assert_eq!(fee_for(0.0), 0.0);
        assert_eq!(fee_for(-5.0), 0.0);
        assert_eq!(fee_for(f64::NAN), 0.0);
    }

    #[test]
    fn slippage_is_adverse() {
        let buy = fill_price("buy", 100.0);
        let sell = fill_price("sell", 100.0);
        assert!(buy > 100.0 && sell < 100.0);
        assert!((buy - 100.05).abs() < 1e-9);
        assert!((sell - 99.95).abs() < 1e-9);
        assert_eq!(fill_price("hold", 100.0), 100.0);
    }
}
