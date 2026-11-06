use axum::{routing::get, Router};
use tower_http::services::ServeDir;
use tracing_subscriber::EnvFilter;

mod auth;
mod agents;
mod backtest;
mod coinbase;
mod dashboard;
mod exchanges;
mod kraken;
mod markets;
mod orchestrator;
mod orders;
mod paper;
mod risk;
mod state;

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard::dashboard_handler))
        .route("/api/markets", get(markets::markets_handler))
        .route("/api/orders", get(orders::orders_handler))
        .route("/api/agents", get(orchestrator::agents_handler))
        .nest_service("/static", ServeDir::new("static"));
    let addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:3000".to_string());
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
