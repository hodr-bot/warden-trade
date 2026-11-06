//! Paper trading ledger (paper only).
//!
//! In-memory record of paper orders. No network, no secrets, no I/O.
//! Every placement goes through [`crate::risk::check_order`] first.

use crate::risk::{check_order, OrderIntent, RiskLimits};

/// A single paper order kept in memory.
#[derive(Debug, Clone)]
pub struct PaperOrder {
    /// Unique ledger id (e.g. `"paper-1"`).
    pub id: String,
    /// Trading symbol, e.g. `"BTC-USD"`.
    pub symbol: String,
    /// Order side, e.g. `"buy"` or `"sell"`.
    pub side: String,
    /// Order quantity in base currency.
    pub qty: f64,
    /// Limit price in quote currency.
    pub price: f64,
    /// Order status, e.g. `"placed"`.
    pub status: String,
    /// RFC3339 creation timestamp.
    pub created_at: String,
}

/// In-memory ledger of paper orders.
#[derive(Debug, Default)]
pub struct PaperLedger {
    orders: Vec<PaperOrder>,
}

impl PaperLedger {
    /// Create an empty ledger.
    pub fn new() -> Self {
        Self { orders: Vec::new() }
    }

    /// Validate `intent` against `limits` via [`check_order`] and, on
    /// success, record the order and return its id.
    ///
    /// Returns `Err(String)` with the rejection reason when the
    /// risk check fails.
    pub fn place(&mut self, limits: &RiskLimits, intent: OrderIntent) -> Result<String, String> {
        check_order(limits, &intent)?;
        let id = format!("paper-{}", self.orders.len() + 1);
        self.orders.push(PaperOrder {
            id: id.clone(),
            symbol: intent.symbol,
            side: intent.side,
            qty: intent.qty,
            price: intent.price,
            status: "placed".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
        });
        Ok(id)
    }

    /// Return a clone of all recorded orders.
    pub fn list(&self) -> Vec<PaperOrder> {
        self.orders.clone()
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
    fn valid_order_is_placed() {
        let mut ledger = PaperLedger::new();
        let id = ledger
            .place(&test_limits(), test_intent(1.0, 100.0))
            .expect("valid order should be placed");
        assert!(!id.is_empty());
        let orders = ledger.list();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].id, id);
        assert_eq!(orders[0].symbol, "BTC-USD");
        assert_eq!(orders[0].status, "placed");
    }

    #[test]
    fn kill_switch_rejects_order() {
        let mut ledger = PaperLedger::new();
        let mut limits = test_limits();
        limits.kill_switch = true;
        let result = ledger.place(&limits, test_intent(1.0, 100.0));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("kill_switch"));
        assert!(ledger.list().is_empty());
    }

    #[test]
    fn oversize_order_is_rejected() {
        let mut ledger = PaperLedger::new();
        // notional = 100 * 500 = 50_000 > 10_000 max
        let result = ledger.place(&test_limits(), test_intent(100.0, 500.0));
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds max_order_notional"));
        assert!(ledger.list().is_empty());
    }
}
