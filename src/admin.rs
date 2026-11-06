//! Admin tenant listing (stub, paper trading only).
//!
//! Billing and invoicing are out of scope until MiCA clearance.

use axum::Json;
use serde::Serialize;

/// Minimal tenant summary for the admin listing.
#[derive(Debug, Clone, Serialize)]
pub struct TenantSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

/// Axum handler returning an empty tenant list (stub, no network).
pub async fn admin_tenants_handler() -> Json<Vec<TenantSummary>> {
    Json(vec![])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handler_returns_empty_list() {
        let Json(tenants) = admin_tenants_handler().await;
        assert!(tenants.is_empty());
    }
}
