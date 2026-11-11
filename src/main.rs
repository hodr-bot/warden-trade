use axum::{routing::get, routing::post, Router};
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tracing_subscriber::EnvFilter;

mod admin;
mod advisor;
mod auth;
mod auth_routes;
mod agents;
mod autonomy;
mod backtest;
mod calibration;
mod coinbase;
mod dashboard;
mod equity;
mod exits;
mod exchanges;
mod fees;
mod kraken;
mod lab;
mod live;
mod livetrade;
mod mail;
mod markets;
mod news;
mod orchestrator;
mod orders;
mod paper;
mod ratelimit;
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
    let limiter = std::sync::Arc::new(ratelimit::RateLimiter::from_env());
    // Public pages get their own looser bucket: profile views must never
    // burn the auth budget that guards Argon2. Same 429 + Retry-After shape.
    let public_limiter = std::sync::Arc::new(ratelimit::RateLimiter::new(120, 60));
    let public_pages = Router::new()
        .route("/u/:id", get(auth_routes::public_profile_handler))
        .route_layer(axum::middleware::from_fn_with_state(
            public_limiter,
            ratelimit::limit_auth,
        ));
    // Auth endpoints burn an Argon2 hash per attempt: they get their own
    // per-IP sliding window so unauthenticated CPU spend stays bounded.
    // (429 + Retry-After when over budget.)
    let auth_api = Router::new()
        .route("/login", post(auth_routes::login_handler))
        .route("/signup", post(auth_routes::signup_handler))
        .route("/change-password", post(auth_routes::change_password_handler))
        .route("/verify/resend", post(auth_routes::resend_verification_handler))
        .route("/reset/request", post(auth_routes::request_reset_handler))
        .route("/reset/confirm", post(auth_routes::confirm_reset_handler))
        .route("/totp/setup", post(auth_routes::setup_totp_handler))
        .route("/totp/confirm", post(auth_routes::confirm_totp_handler))
        .route("/totp/verify", post(auth_routes::verify_totp_handler))
        .route("/totp/disable", post(auth_routes::disable_totp_handler))
        .route("/profile", post(auth_routes::update_profile_handler))
        .route("/account", axum::routing::delete(auth_routes::delete_account_handler))
        .route_layer(axum::middleware::from_fn_with_state(
            limiter,
            ratelimit::limit_auth,
        ));
    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(dashboard::dashboard_handler))
        .route("/login", get(auth_routes::login_page_handler))
        .route("/signup", get(auth_routes::signup_page_handler))
        .route("/verify", get(auth_routes::verify_page_handler))
        .route("/reset", get(auth_routes::reset_page_handler))
        .route("/account", get(auth_routes::account_page_handler))
        .route("/markets", get(markets::markets_page_handler))
        .route("/orders", get(orders::orders_page_handler))
        .route("/api/markets", get(markets::markets_handler))
        .route("/api/orders", get(orders::orders_handler).post(orders::create_order_handler))
        .route("/api/backtest", get(backtest::backtest_handler))
        .nest("/api/auth", auth_api)
        .merge(public_pages)
        .route("/api/auth/logout", post(auth_routes::logout_handler))
        .route("/api/reconcile", get(reconcile::reconcile_handler))
        .route("/api/agents", get(orchestrator::agents_handler))
        .route("/api/agents/run", post(orchestrator::run_cycle_handler))
        .route("/api/autonomy", get(autonomy::autonomy_handler))
        .route("/api/advisor", get(advisor::advisor_handler))
        .route("/api/lab/evaluate", axum::routing::post(lab::evaluate_handler))
        .route("/api/lab/journal", get(lab::journal_handler))
        .route("/api/live/status", get(kraken::live_status_handler))
        .route(
            "/api/wallet/watch",
            axum::routing::put(wallet::set_watch_handler)
                .delete(wallet::clear_watch_handler),
        )
        .route("/api/live/stream", get(live::stream_handler))
        .route("/api/admin/tenants", get(admin::admin_tenants_handler))
        .nest_service("/static", ServeDir::new("static"))
        // Baseline hardening headers on every response, static files
        // included. No CSP yet: the UI legitimately uses inline scripts and
        // the unpkg htmx CDN, so a strict policy would break the app; the
        // three headers below carry no such conflict.
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::X_FRAME_OPTIONS,
            axum::http::HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::REFERRER_POLICY,
            axum::http::HeaderValue::from_static("same-origin"),
        ))
        .with_state(state.clone());
    if scheduler::scheduler_enabled() {
        // Trailing-stop peaks survive restarts via `exit_peaks`: seed the
        // in-memory map ONCE here, before either evaluation loop starts.
        // (Later seeding would clobber live peaks with stale rows.)
        // Best effort: a seeding failure only means peaks restart fresh.
        match scheduler::solo_tenant(&state.pool).await {
            Some(t) => crate::exits::seed_exit_peaks(&state.pool, t).await,
            None => tracing::info!("no tenant yet: exit peaks start empty"),
        }
        scheduler::spawn(state.clone());
        scheduler::spawn_exit_watch(state.clone());
        // J3: counsel on its own task — the trading tick never waits on
        // the LLM. No-op without LLM_API_KEY + LLM_MODEL.
        advisor::spawn_advisor(state.clone());
        // J3: adopt the legacy env watch address onto the solo tenant
        // once, so the owner's view survives the per-tenant migration.
        wallet::seed_from_env(&state.pool).await;
    } else {
        tracing::info!("scheduler off (SCHEDULER_ENABLED=false)");
    }
    // Real-time marks feed (Kraken public WS, keyless). Runs alongside the
    // scheduler; pages degrade to 30s HTMX polling if the feed is down.
    live::spawn_feed();
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    tracing::info!("listening on {addr}");
    // ConnectInfo feeds the auth rate limiter with the TCP peer address
    // (used only when no Cloudflare/XFF header identifies the client).
    let svc = app.into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, svc).await.unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn timestamp_is_rfc3339() {
        let now = chrono::Utc::now().to_rfc3339();
        assert!(now.contains('T'));
    }
}
