//! Admin tenant listing (paper trading only).
//!
//! Billing and invoicing are out of scope until MiCA clearance.

use axum::{extract::State, Json};
use serde::Serialize;

use crate::state::AppState;

/// Minimal tenant summary for the admin listing.
#[derive(Debug, Clone, Serialize)]
pub struct TenantSummary {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

/// Axum handler returning tenants from Postgres.
///
/// Falls back to an empty list (with a warning log) when the database
/// is unreachable, so the server stays up in degraded mode.
pub async fn admin_tenants_handler(State(state): State<AppState>) -> Json<Vec<TenantSummary>> {
    let rows = sqlx::query(
        "SELECT id::TEXT AS id, name, created_at FROM tenants ORDER BY created_at DESC LIMIT 100",
    )
    .fetch_all(&state.pool)
    .await;
    match rows {
        Ok(rows) => {
            use sqlx::Row;
            Json(
                rows.iter()
                    .map(|r| TenantSummary {
                        id: r.try_get("id").unwrap_or_default(),
                        name: r.try_get("name").unwrap_or_default(),
                        created_at: r
                            .try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_default(),
                    })
                    .collect(),
            )
        }
        Err(e) => {
            tracing::warn!("admin tenants fallback to empty list: {e}");
            Json(vec![])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handler_falls_back_to_empty_without_db() {
        let state = AppState {
            pool: sqlx::PgPool::connect_lazy("postgres://127.0.0.1:1/none")
                .expect("test lazy pool"),
            jwt_secret: "test".to_string(),
        };
        let Json(tenants) = admin_tenants_handler(State(state)).await;
        assert!(tenants.is_empty());
    }
}
