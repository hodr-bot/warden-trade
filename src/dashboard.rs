use askama::Template;
use axum::extract::State;
use chrono::Utc;

use crate::state::AppState;

/// Askama template for the paper-trading dashboard overview.
#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub capital_total: String,
    pub portfolio_value: String,
    pub pnl_realized: String,
    pub pnl_unrealized: String,
    pub trades_count: String,
    pub fees_paid: String,
    pub drawdown: String,
    pub win_rate: String,
    pub generated_at: String,
}

/// Backend handler building the dashboard from live Postgres counts.
///
/// `trades_count` is the real `paper_orders` row count; other money
/// fields stay at `0` until portfolio valuation lands. Degrades to
/// zeros (with a warning) when the database is unreachable.
/// `generated_at` is always the current backend time.
pub async fn dashboard_handler(State(state): State<AppState>) -> axum::response::Html<String> {
    let (trades_count, tenants_count) = match sqlx::query(
        "SELECT (SELECT COUNT(*) FROM paper_orders) AS orders, (SELECT COUNT(*) FROM tenants) AS tenants",
    )
    .fetch_one(&state.pool)
    .await
    {
        Ok(row) => {
            use sqlx::Row;
            let orders: i64 = row.try_get("orders").unwrap_or(0);
            let tenants: i64 = row.try_get("tenants").unwrap_or(0);
            (orders.to_string(), tenants.to_string())
        }
        Err(e) => {
            tracing::warn!("dashboard counts degraded: {e}");
            ("0".to_string(), "0".to_string())
        }
    };
    let _ = tenants_count;
    let template = DashboardTemplate {
        capital_total: "0".to_string(),
        portfolio_value: "0".to_string(),
        pnl_realized: "0".to_string(),
        pnl_unrealized: "0".to_string(),
        trades_count,
        fees_paid: "0".to_string(),
        drawdown: "0".to_string(),
        win_rate: "0%".to_string(),
        generated_at: Utc::now().to_rfc3339(),
    };
    axum::response::Html(template.render().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_template_renders_warden() {
        let template = DashboardTemplate {
            capital_total: "10000".to_string(),
            portfolio_value: "10000".to_string(),
            pnl_realized: "0".to_string(),
            pnl_unrealized: "0".to_string(),
            trades_count: "0".to_string(),
            fees_paid: "0".to_string(),
            drawdown: "0%".to_string(),
            win_rate: "0%".to_string(),
            generated_at: "now".to_string(),
        };
        let rendered = template.render().expect("template renders");
        assert!(
            rendered.contains("Warden"),
            "rendered dashboard should contain Warden"
        );
    }

    #[tokio::test]
    async fn handler_degrades_without_db() {
        let state = AppState {
            pool: sqlx::PgPool::connect_lazy("postgres://127.0.0.1:1/none")
                .expect("test lazy pool"),
            jwt_secret: "test".to_string(),
        };
        let axum::response::Html(body) = dashboard_handler(State(state)).await;
        assert!(body.contains("Warden"));
    }
}
