//! Paper orders endpoint (paper only, Postgres-backed).
//!
//! `GET /api/orders` lists persisted paper orders.
//! `POST /api/orders` validates via [`crate::risk::check_order_full`] then persists.
//! No network, no secrets beyond the server-side pool.

use askama::Template;
use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;

use crate::paper::PaperOrder;
use crate::risk::{check_order, check_order_full, load_limits, OrderIntent};
use crate::state::{AppState, Tenant};

/// Incoming order request (JSON body for `POST /api/orders`).
#[derive(Debug, Clone, Deserialize)]
pub struct CreateOrderRequest {
    pub symbol: String,
    pub side: String,
    pub qty: f64,
    /// Accepted for API compatibility but IGNORED (J2 "prix client"): the
    /// fill executes at the server's live mark, never at a posted number.
    #[allow(dead_code)]
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
                        fee: r.try_get("fee").unwrap_or_default(),
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

/// Validate via the risk engine then persist a paper order for `tenant_id`.
///
/// Shared by [`create_order_handler`] (user-driven) and the autonomous
/// scheduler (signal-driven): same gate, same insert, same `201` shape
/// on success. `idempotency_key` should always be set by callers; a
/// duplicate key surfaces as a unique violation from Postgres.
///
/// `price` must be server-resolved (feed mark / last close) — never the
/// caller-posted value (J2 "prix client"): fills execute at the market,
/// not at a number typed into a form.
///
/// J2 atomicity: the risk gate and the insert run under the state's
/// `order_gate` mutex, so a scheduler tick and a manual order racing
/// each other cannot both pass the gate on the same ledger state
/// check-then-insert was racy). J2 fail-closed: with an unreachable
/// database buys are denied by the gate (`400 risk data unavailable`)
/// and sells fail on the insert (generic `500` — nothing is recorded
/// without a ledger). Insert failures never leak database internals.
pub async fn place_paper_order(
    state: &AppState,
    tenant_id: uuid::Uuid,
    exchange: &str,
    symbol: &str,
    side: &str,
    qty: f64,
    price: f64,
    idempotency_key: Option<&str>,
) -> Result<PaperOrder, (StatusCode, String)> {
    let _gate = state.order_gate.lock().await;
    let pool = &state.pool;
    // J3 cost parity: the fill executes post-slippage and pays the taker
    // fee, exactly like the backtester assumes ([`crate::fees`]).
    let exec_price = crate::fees::fill_price(side, price);
    let fee = crate::fees::fee_for(exec_price * qty);
    let intent = OrderIntent {
        symbol: symbol.to_string(),
        side: side.to_string(),
        qty,
        price: exec_price,
    };
    // Risk gate: per-tenant limits from Postgres. The engine never writes
    // limits; agents have no path to change them. Marks feed the
    // unrealized leg of the daily-loss gate; an empty map only skips
    // that leg (never hallucinates prices).
    let marks = crate::live::current_marks();
    let limits = load_limits(pool, tenant_id).await;
    check_order_full(pool, tenant_id, &limits, &intent, &marks)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let row = sqlx::query(
        "INSERT INTO paper_orders (tenant_id, exchange, symbol, side, qty, price, fee, status, idempotency_key)
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'filled', $8)
         RETURNING id::TEXT AS id, exchange, symbol, side, qty, price, fee, status, created_at",
    )
    .bind(tenant_id)
    .bind(exchange)
    .bind(symbol)
    .bind(side)
    .bind(qty)
    .bind(exec_price)
    .bind(fee)
    .bind(idempotency_key)
    .fetch_one(pool)
    .await
    .map_err(|e| {
        if let Some(db) = e.as_database_error() {
            if db.is_unique_violation() {
                return (StatusCode::CONFLICT, "duplicate idempotency_key".to_string());
            }
        }
        tracing::warn!("paper order insert failed: {e}");
        (StatusCode::INTERNAL_SERVER_ERROR, "order could not be recorded".to_string())
    })?;

    use sqlx::Row;
    Ok(PaperOrder {
        id: row.try_get("id").unwrap_or_default(),
        exchange: row.try_get("exchange").unwrap_or_else(|_| "paper".to_string()),
        symbol: row.try_get("symbol").unwrap_or_default(),
        side: row.try_get("side").unwrap_or_default(),
        qty: row.try_get("qty").unwrap_or_default(),
        price: row.try_get("price").unwrap_or_default(),
        fee: row.try_get("fee").unwrap_or_default(),
        status: row.try_get("status").unwrap_or_else(|_| "filled".to_string()),
        created_at: row
            .try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
            .map(|dt| dt.to_rfc3339())
            .unwrap_or_default(),
    })
}

/// Validate via the risk engine then persist a paper order for the authenticated tenant.
///
/// Requires `Authorization: Bearer *** via [`Tenant`].
/// Rejections from [`check_order`] become `400`; DB failures become `500`.
/// Success returns `201` with the persisted order.
///
/// J2 ("prix client"): the posted `price` is IGNORED — the fill executes
/// at the server's live mark for the symbol
/// ([`crate::risk::resolve_execution_price`]). A caller-posted number is
/// never a market; trusting it would let a stale or malicious quote set
/// the ledger. When the feed has no fresh mark the order is refused
/// (`503`, feed blind) instead of inventing a price.
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
    let price = crate::risk::resolve_execution_price(&crate::live::current_marks(), &req.symbol)
        .ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "no live price for symbol: feed blind, try again".to_string(),
        ))?;
    let order = place_paper_order(
        &state,
        tenant_id,
        &req.exchange,
        &req.symbol,
        &side,
        req.qty,
        price,
        req.idempotency_key.as_deref(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(order)))
}

/// Askama template for the orders HTML page (same rows as the API).
#[derive(Template)]
#[template(path = "orders.html")]
pub struct OrdersTemplate {
    pub orders: Vec<PaperOrder>,
    pub generated_at: String,
}

/// Server-rendered orders page, scoped to the caller's tenant.
///
/// Requires login ([`AuthPage`] redirects browsers to `/login`).
/// Falls back to an empty table when the database is unreachable.
pub async fn orders_page_handler(
    page: crate::state::AuthPage,
    State(state): State<AppState>,
) -> axum::response::Html<String> {
    let tenant_id: uuid::Uuid = match page.0.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return axum::response::Html("invalid tenant".to_string()),
    };
    let rows = sqlx::query(
        "SELECT id::TEXT AS id, exchange, symbol, side, qty, price, COALESCE(fee, 0) AS fee, status, created_at
         FROM paper_orders WHERE tenant_id = $1 ORDER BY created_at DESC LIMIT 100",
    )
    .bind(tenant_id)
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
                    fee: row.try_get("fee").unwrap_or(0.0),
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
        generated_at: crate::dashboard::short_now(),
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
            order_gate: Default::default(),
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
        let page = crate::state::AuthPage(crate::state::Tenant {
            tenant_id: "00000000-0000-0000-0000-000000000001".to_string(),
            user_id: "u".to_string(),
        });
        let axum::response::Html(body) =
            orders_page_handler(page, State(test_state_unreachable())).await;
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
        assert!(check_order(&crate::risk::RiskLimits::conservative_default(), &intent).is_err());
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
