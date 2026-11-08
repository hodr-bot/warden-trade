use axum::{routing::get, routing::post, Router};
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

mod admin;
mod advisor;
mod auth;
mod auth_routes;
mod agents;
mod autonomy;
mod backtest;
mod coinbase;
mod dashboard;
mod equity;
mod exits;
mod exchanges;
mod kraken;
mod live;
mod livetrade;
mod markets;
mod news;
mod orchestrator;
mod orders;
mod paper;
mod reconcile;
mod risk;
mod scheduler;
mod state;
mod strategies;
mod wallet;

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let (state, addr) = state::AppState::from_env();
    if let Err(e) = sqlx::migrate!("./migrations").run(&state.pool).await {
        tracing::warn!("migrations not applied, running degraded: {e}");
    }
    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard::dashboard_handler))
        .route("/login", get(auth_routes::login_page_handler))
        .route("/markets", get(markets::markets_page_handler))
        .route("/orders", get(orders::orders_page_handler))
        .route("/api/markets", get(markets::markets_handler))
        .route("/api/orders", get(orders::orders_handler).post(orders::create_order_handler))
        .route("/api/backtest", get(backtest::backtest_handler))
        .route("/api/auth/login", post(auth_routes::login_handler))
        .route("/api/auth/logout", post(auth_routes::logout_handler))
        .route("/api/reconcile", get(reconcile::reconcile_handler))
        .route("/api/agents", get(orchestrator::agents_handler))
        .route("/api/agents/run", post(orchestrator::run_cycle_handler))
        .route("/api/autonomy", get(autonomy::autonomy_handler))
        .route("/api/advisor", get(advisor::advisor_handler))
        .route("/api/live/status", get(kraken::live_status_handler))
        .route("/api/live/stream", get(live::stream_handler))
        .route("/api/admin/tenants", get(admin::admin_tenants_handler))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state.clone());
    if scheduler::scheduler_enabled() {
        scheduler::spawn(state);
    } else {
        tracing::info!("scheduler off (SCHEDULER_ENABLED=false)");
    }
    // Real-time marks feed (Kraken public WS, keyless). Runs alongside the
    // scheduler; pages degrade to 30s HTMX polling if the feed is down.
    live::spawn_feed();
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!("listening on {addr}");
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamp_is_rfc3339() {
        let now = chrono::Utc::now().to_rfc3339();
        assert!(now.contains('T'));
    }
}
