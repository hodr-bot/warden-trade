//! Paper orders endpoint (paper only).
//!
//! No network, no secrets, no I/O. Returns the contents of an empty
//! in-memory [`crate::paper::PaperLedger`] as JSON.

use axum::Json;

use crate::paper::{PaperLedger, PaperOrder};

/// Return all paper orders from an empty [`PaperLedger`] as JSON.
///
/// Always returns an empty list: the ledger is created fresh per call.
pub async fn orders_handler() -> Json<Vec<PaperOrder>> {
    let ledger = PaperLedger::new();
    Json(ledger.list())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn orders_handler_returns_empty_list() {
        let Json(orders) = orders_handler().await;
        assert!(
            orders.is_empty(),
            "fresh PaperLedger should yield an empty list"
        );
    }
}
