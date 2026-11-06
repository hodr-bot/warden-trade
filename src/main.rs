use axum::{response::Html, routing::get, Router};
use tracing_subscriber::EnvFilter;

mod auth;
mod state;

async fn health() -> &'static str {
    "ok"
}

async fn dashboard() -> Html<String> {
    let now = chrono::Utc::now().to_rfc3339();
    Html(format!(
        r#"<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Warden Trade</title></head>
<body style="background:#0b0e14;color:#e6edf3;font-family:system-ui">
<main style="max-width:960px;margin:40px auto;padding:0 20px">
<h1>Warden Trade — Phase 1 bootstrap</h1>
<p>Backend reachable. Timestamp: {now}</p>
<p>Paper trading only. No real funds.</p>
</main></body></html>"#
    ))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard));
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
