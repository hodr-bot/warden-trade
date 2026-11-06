//! Paper orders endpoint (paper only, Postgres-backed).
//!
//! `GET /api/orders` lists persisted paper orders.
//! `POST /api/orders` validates via [`crate::risk::check_order`] then persists.
//! No network, no secrets beyond the server-side pool.

use askama::Template;
use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;

use crate::paper::PaperOrder;
use crate::risk::{check_order, OrderIntent, RiskLimits};
use crate::state::{AppState, Tenant};

/// Default paper risk limits (same bounds as autonomy reports).
fn default_limits() -> RiskLimits {
    RiskLimits {
        max_order_notional: 10_000.0,
        max_exposure_per_asset: 50_000.0,
        max_daily_loss: 1_000.0,
        max_open_orders: 10,
        kill_switch: false,
    }
}

/// Incoming order request (JSON body for `POST /api/orders`).
#[derive(Debug, Clone, Deserialize)]
pub struct CreateOrderRequest {
    pub symbol: String,
    pub side: String,
    pub qty: f64,
    pub price: f64,
    #[serde(default = "default_exchange_name")]
    pub exchange: String,
    pub idempotency_key: Option<String>,
}

fn default_exchange_name() -> String {
    "paper".to_string()
}

/// Ensure a tenant exists and return its id.
///
/// Solo-admin first: reuse the oldest tenant, creating `solo-admin`
/// when the table is empty. (Used by tests and bootstrap; request
/// handlers use the authenticated [`Tenant`] instead.)
async fn ensure_tenant(pool: &sqlx::PgPool) -> Result<uuid::Uuid, sqlx::Error> {
    if let Some(row) = sqlx::query("SELECT id FROM tenants ORDER BY created_at ASC LIMIT 1")
        .fetch_optional(pool)
        .await?
    {
        use sqlx::Row;
        return row.try_get("id");
    }
    let row = sqlx::query("INSERT INTO tenants (name) VALUES ('solo-admin') RETURNING id")
        .fetch_one(pool)
        .await?;
    use sqlx::Row;
    row.try_get("id")
}

/// Return the authenticated tenant's paper orders as JSON (newest first, max 100).
///
/// Requires `Authorization: Bearer <JWT>` via [`Tenant`]; rows are
/// filtered on `tenant_id`. Falls back to an empty list when the
/// database is unreachable.
pub async fn orders_handler(
    tenant: Tenant,
    State(state): State<AppState>,
) -> Json<Vec<PaperOrder>> {
    let tenant_id: uuid::Uuid = match tenant.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Json(vec![]),
    };
    let rows = sqlx::query(
        "SELECT id::TEXT AS id, exchange, symbol, side, qty, price, status, created_at
         FROM paper_orders WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 100",
    )
    .bind(tenant_id)
    .fetch_all(&state.pool)
    .await;
    match rows {
        Ok(rows) => {
            use sqlx::Row;
            Json(
                rows.iter()
                    .map(|r| PaperOrder {
                        id: r.try_get("id").unwrap_or_default(),
                        exchange: r.try_get("exchange").unwrap_or_else(|_| "paper".to_string()),
                        symbol: r.try_get("symbol").unwrap_or_default(),
                        side: r.try_get("side").unwrap_or_default(),
                        qty: r.try_get("qty").unwrap_or_default(),
                        price: r.try_get("price").unwrap_or_default(),
                        status: r.try_get("status").unwrap_or_else(|_| "filled".to_string()),
                        created_at: r
                            .try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_default(),
                    })
                    .collect(),
            )
        }
        Err(e) => {
            tracing::warn!("orders fallback to empty list: {e}");
            Json(vec![])
        }
    }
}

/// Validate via the risk engine then persist a paper order for the authenticated tenant.
///
/// Requires `Authorization: Bearer <JWT>` via [`Tenant`].
/// Rejections from [`check_order`] become `400`; DB failures become `500`.
/// Success returns `201` with the persisted order.
pub async fn create_order_handler(
    tenant: Tenant,
    State(state): State<AppState>,
    Json(req): Json<CreateOrderRequest>,
) -> Result<(StatusCode, Json<PaperOrder>), (StatusCode, String)> {
    let tenant_id: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid tenant".to_string()))?;
    let side = req.side.to_ascii_lowercase();
    if side != "buy" && side != "sell" {
        return Err((StatusCode::BAD_REQUEST, "side must be buy or sell".to_string()));
    }
    let intent = OrderIntent {
        symbol: req.symbol.clone(),
        side: side.clone(),
        qty: req.qty,
        price: req.price,
    };
    check_order(&default_limits(), &intent)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let row = sqlx::query(
        "INSERT INTO paper_orders (tenant_id, exchange, symbol, side, qty, price, status, idempotency_key)
         VALUES ($1, $2, $3, $4, $5, $6, 'filled', $7)
         RETURNING id::TEXT AS id, exchange, symbol, side, qty, price, status, created_at",
    )
    .bind(tenant_id)
    .bind(&req.exchange)
    .bind(&req.symbol)
    .bind(&side)
    .bind(req.qty)
    .bind(req.price)
    .bind(&req.idempotency_key)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| {
        if let Some(db) = e.as_database_error() {
            if db.is_unique_violation() {
                return (StatusCode::CONFLICT, "duplicate idempotency_key".to_string());
            }
        }
        (StatusCode::INTERNAL_SERVER_ERROR, format!("insert failed: {e}"))
    })?;

    use sqlx::Row;
    let order = PaperOrder {
        id: row.try_get("id").unwrap_or_default(),
        exchange: row.try_get("exchange").unwrap_or_else(|_| "paper".to_string()),
        symbol: row.try_get("symbol").unwrap_or_default(),
        side: row.try_get("side").unwrap_or_default(),
        qty: row.try_get("qty").unwrap_or_default(),
        price: row.try_get("price").unwrap_or_default(),
        status: row.try_get("status").unwrap_or_else(|_| "filled".to_string()),
        created_at: row
            .try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
    };
    Ok((StatusCode::CREATED, Json(order)))
}

/// Askama template for the orders HTML page (same rows as the API).
#[derive(Template)]
#[template(path = "orders.html")]
pub struct OrdersTemplate {
    pub orders: Vec<PaperOrder>,
    pub generated_at: String,
}

/// Server-rendered orders page for the solo-admin (all tenants, newest 100).
///
/// Same-origin HTML page, no JWT needed; the JSON API stays tenant-scoped
/// behind [`Tenant`]. Falls back to an empty table when the database is
/// unreachable.
pub async fn orders_page_handler(
    State(state): State<AppState>,
) -> axum::response::Html<String> {
    let rows = sqlx::query(
        "SELECT id::TEXT AS id, exchange, symbol, side, qty, price, status, created_at
         FROM paper_orders ORDER BY created_at DESC LIMIT 100",
    )
    .fetch_all(&state.pool)
    .await;
    let orders = match rows {
        Ok(rows) => rows
            .iter()
            .map(|row| {
                use sqlx::Row;
                let created_at: chrono::DateTime<chrono::Utc> =
                    row.try_get("created_at").unwrap_or_default();
                PaperOrder {
                    id: row.try_get("id").unwrap_or_default(),
                    exchange: row.try_get("exchange").unwrap_or_else(|_| "paper".to_string()),
                    symbol: row.try_get("symbol").unwrap_or_default(),
                    side: row.try_get("side").unwrap_or_default(),
                    qty: row.try_get("qty").unwrap_or(0.0),
                    price: row.try_get("price").unwrap_or(0.0),
                    status: row.try_get("status").unwrap_or_default(),
                    created_at: created_at.to_rfc3339(),
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!("orders page degraded: {e}");
            vec![]
        }
    };
    let template = OrdersTemplate {
        orders,
        generated_at: chrono::Utc::now().to_rfc3339(),
    };
    axum::response::Html(template.render().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state_unreachable() -> AppState {
        AppState {
            pool: sqlx::PgPool::connect_lazy("postgres://127.0.0.1:1/none")
                .expect("test lazy pool"),
            jwt_secret: "test".to_string(),
        }
    }

    fn test_tenant() -> Tenant {
        Tenant {
            tenant_id: "00000000-0000-0000-0000-000000000001".to_string(),
            user_id: "00000000-0000-0000-0000-000000000002".to_string(),
        }
    }

    #[tokio::test]
    async fn orders_handler_falls_back_to_empty_without_db() {
        let Json(orders) = orders_handler(test_tenant(), State(test_state_unreachable())).await;
        assert!(orders.is_empty());
    }

    #[tokio::test]
    async fn invalid_tenant_returns_empty() {
        let bad = Tenant { tenant_id: "not-a-uuid".to_string(), user_id: "u".to_string() };
        let Json(orders) = orders_handler(bad, State(test_state_unreachable())).await;
        assert!(orders.is_empty());
    }

    #[tokio::test]
    async fn orders_page_degrades_to_empty_table_without_db() {
        let axum::response::Html(body) =
            orders_page_handler(State(test_state_unreachable())).await;
        assert!(body.contains("Orders"));
    }

    #[test]
    fn oversize_order_fails_risk_check() {
        let intent = OrderIntent {
            symbol: "BTC-USD".to_string(),
            side: "buy".to_string(),
            qty: 100.0,
            price: 500.0,
        };
        assert!(check_order(&default_limits(), &intent).is_err());
    }

    #[test]
    fn request_defaults_to_paper_exchange() {
        let req: CreateOrderRequest =
            serde_json::from_str(r#"{"symbol":"BTC-USD","side":"buy","qty":1.0,"price":100.0}"#)
                .expect("request parses");
        assert_eq!(req.exchange, "paper");
        assert!(req.idempotency_key.is_none());
    }
}
