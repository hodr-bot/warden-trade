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
}
