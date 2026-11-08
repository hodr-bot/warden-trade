//! Live-trading kill switch (real funds gate).
//!
//! Paper trading stays the only active path. NOTHING in this codebase can
//! move real funds today: there is deliberately no execution client, no key
//! handling, and no order-sending code — only this gate and a preview
//! ticket. Sending a real order will require, together, all of:
//!
//! 1. `LIVE_TRADING_ENABLED=true` in the server environment (default off,
//!    exact match, never from a request or the database);
//! 2. an explicit per-order confirmation (a future authenticated endpoint,
//!    not yet built on purpose);
//! 3. the unchanged [`crate::risk`] limits, which the bot itself cannot
//!    modify or disable.
//!
//! [`prepare_live_ticket`] enforces (1) first and always refuses while the
//! switch is off, so every caller gets a denial-by-default.

/// Whether real execution is armed. Exact `"true"` only; anything else —
/// unset, empty, `"1"`, `"yes"` — means off.
pub fn live_trading_enabled() -> bool {
    matches!(
        std::env::var("LIVE_TRADING_ENABLED").as_deref(),
        Ok("true")
    )
}

/// A preview of an order that WOULD be sent once live trading is armed.
/// No execution capability is attached to this struct by construction.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveTicket {
    pub symbol: String,
    pub side: String,
    pub qty: f64,
    pub notional_usd: f64,
    pub warning: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LiveRefusal {
    /// The master switch is off. This is the normal state.
    Disabled,
    UnknownSymbol(String),
    BadSide(String),
    BadQty,
}

impl std::fmt::Display for LiveRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LiveRefusal::Disabled => write!(f, "live trading is disabled (LIVE_TRADING_ENABLED!=true)"),
            LiveRefusal::UnknownSymbol(_) => write!(f, "unknown symbol"),
            LiveRefusal::BadSide(_) => write!(f, "side must be buy or sell"),
            LiveRefusal::BadQty => write!(f, "quantity must be positive and finite"),
        }
    }
}

/// Assets the future live path may quote. Closed set: anything else is
/// refused before it can reach an exchange.
const LIVE_SYMBOLS: &[&str] = &["BTC-USD", "ETH-USD", "SOL-USD"];

/// Validate a live-order request into a preview ticket. Always refuses with
/// [`LiveRefusal::Disabled`] unless the master switch is armed; validation
/// errors are checked after the switch so no request shape can leak around
/// the gate.
pub fn prepare_live_ticket(
    symbol: &str,
    side: &str,
    qty: f64,
    mark_price: f64,
) -> Result<LiveTicket, LiveRefusal> {
    if !live_trading_enabled() {
        return Err(LiveRefusal::Disabled);
    }
    if !LIVE_SYMBOLS.contains(&symbol) {
        return Err(LiveRefusal::UnknownSymbol(symbol.to_string()));
    }
    if side != "buy" && side != "sell" {
        return Err(LiveRefusal::BadSide(side.to_string()));
    }
    if !qty.is_finite() || qty <= 0.0 || !mark_price.is_finite() || mark_price <= 0.0 {
        return Err(LiveRefusal::BadQty);
    }
    Ok(LiveTicket {
        symbol: symbol.to_string(),
        side: side.to_string(),
        qty,
        notional_usd: qty * mark_price,
        warning: "PREVIEW ONLY — no execution path exists yet",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Env vars are process-global: serialize every test that touches the
    /// switch so parallel runners cannot flip it mid-assertion.
    static SWITCH_LOCK: Mutex<()> = Mutex::new(());

    fn with_switch(val: Option<&str>, f: impl FnOnce()) {
        let _guard = SWITCH_LOCK.lock().unwrap();
        let key = "LIVE_TRADING_ENABLED";
        let prev = std::env::var(key).ok();
        match val {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        f();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn switch_defaults_off_and_rejects_lookalikes() {
        with_switch(None, || assert!(!live_trading_enabled()));
        for v in ["", "1", "yes", "TRUE", " true"] {
            with_switch(Some(v), || assert!(!live_trading_enabled(), "{v:?} must not arm"));
        }
        with_switch(Some("true"), || assert!(live_trading_enabled()));
    }

    #[test]
    fn ticket_refused_while_disabled_even_if_valid() {
        with_switch(None, || {
            assert_eq!(
                prepare_live_ticket("BTC-USD", "buy", 0.001, 75_000.0),
                Err(LiveRefusal::Disabled)
            );
        });
    }

    #[test]
    fn ticket_validates_when_armed() {
        with_switch(Some("true"), || {
            let t = prepare_live_ticket("SOL-USD", "buy", 1.0, 90.0).expect("valid");
            assert_eq!(t.notional_usd, 90.0);
            assert_eq!(
                prepare_live_ticket("DOGE-USD", "buy", 1.0, 1.0),
                Err(LiveRefusal::UnknownSymbol("DOGE-USD".to_string()))
            );
            assert_eq!(
                prepare_live_ticket("BTC-USD", "hold", 1.0, 1.0),
                Err(LiveRefusal::BadSide("hold".to_string()))
            );
            assert_eq!(
                prepare_live_ticket("BTC-USD", "buy", -1.0, 1.0),
                Err(LiveRefusal::BadQty)
            );
            assert_eq!(
                prepare_live_ticket("BTC-USD", "buy", 1.0, f64::INFINITY),
                Err(LiveRefusal::BadQty)
            );
        });
    }
}
