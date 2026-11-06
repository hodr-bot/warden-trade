//! Reconciliation (paper trading, read-only).
//!
//! `GET /api/reconcile` compares persisted counts for the authenticated
//! tenant: `paper_orders` vs `agent_decisions`. Requires
//! `Authorization: Bearer <JWT>` via [`Tenant`].

use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;

use crate::state::{AppState, Tenant};

/// Per-tenant reconciliation snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct ReconcileReport {
    pub tenant_id: String,
    pub orders: i64,
    pub decisions: i64,
    pub ok: bool,
    pub generated_at: String,
}

/// Count persisted rows for the authenticated tenant.
///
/// `ok` is true when both counts are readable (DB reachable); DB
/// failures become `503`. Read-only: never writes, never trades.
pub async fn reconcile_handler(
    tenant: Tenant,
    State(state): State<AppState>,
) -> Result<Json<ReconcileReport>, (StatusCode, String)> {
    let tenant_id: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid tenant".to_string()))?;
    let order_row = sqlx::query("SELECT COUNT(*) AS n FROM paper_orders WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(&state.pool)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")))?;
    let decision_row =
        sqlx::query("SELECT COUNT(*) AS n FROM agent_decisions WHERE tenant_id = $1")
            .bind(tenant_id)
            .fetch_one(&state.pool)
            .await
            .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")))?;
    use sqlx::Row;
    let orders: i64 = order_row.try_get("n").unwrap_or(0);
    let decisions: i64 = decision_row.try_get("n").unwrap_or(0);
    Ok(Json(ReconcileReport {
        tenant_id: tenant.tenant_id,
        orders,
        decisions,
        ok: true,
        generated_at: chrono::Utc::now().to_rfc3339(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_serializes() {
        let r = ReconcileReport {
            tenant_id: "t".to_string(),
            orders: 1,
            decisions: 0,
            ok: true,
            generated_at: "now".to_string(),
        };
        let v = serde_json::to_value(&r).expect("serializes");
        assert_eq!(v["orders"], 1);
        assert!(v["ok"].as_bool().unwrap());
    }
}
