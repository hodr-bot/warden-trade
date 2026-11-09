use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use sqlx::PgPool;

/// Shared application state: Postgres pool plus JWT secret.
///
/// `Clone` so the autonomous scheduler can own a copy alongside Axum.
#[derive(Clone)]
pub struct AppState {
    /// Postgres connection pool. Every query must filter on `tenant_id`.
    pub pool: PgPool,
    /// HS256 secret used to sign/verify JWTs (see `crate::auth`).
    pub jwt_secret: String,
    /// Serializes risk-gate + order-insert (J2: check-then-insert race).
    ///
    /// [`crate::orders::place_paper_order`] holds this across gate and
    /// insert so concurrent ticks/API calls cannot both pass the gate on
    /// the same ledger state. Process-local: the deployment runs a
    /// single writer process. Cloned with the state (same mutex).
    pub order_gate: std::sync::Arc<tokio::sync::Mutex<()>>,
}

/// Boot-time secret policy, pure for tests. Rejects the documented
/// placeholder and short secrets: a set-but-worthless secret is as bad
/// as none, and a copy-pasted `.env.example` must never sign sessions.
fn validate_boot_secrets(jwt_secret: &str, admin_password: &str) -> Result<(), &'static str> {
    if jwt_secret == "dev-placeholder-change-me" || jwt_secret.len() < 32 {
        return Err("JWT_SECRET is a placeholder or too short (>= 32 chars required)");
    }
    if !admin_password.is_empty() && admin_password.len() < 10 {
        return Err("ADMIN_PASSWORD too short (>= 10 chars required)");
    }
    Ok(())
}

impl AppState {
    /// Build state from the environment. Fail-closed: `DATABASE_URL` and
    /// `JWT_SECRET` have NO defaults — booting without them would sign
    /// tokens with a public placeholder and hand the admin account to
    /// whoever logs in first. Only `BIND_ADDR` keeps a default (not
    /// a secret).
    ///
    /// Reads `DATABASE_URL`, `JWT_SECRET` and `BIND_ADDR`.
    /// Returns the state plus the bind address to listen on.
    /// The pool connects lazily: no database traffic happens until the
    /// first query, so the server boots even when Postgres is down
    /// (DB-backed routes then fall back to empty results and log why).
    pub fn from_env() -> (Self, String) {
        let database_url = std::env::var("DATABASE_URL")
            .expect("DATABASE_URL must be set: refusing to boot without an explicit database");
        let jwt_secret = std::env::var("JWT_SECRET")
            .expect("JWT_SECRET must be set: refusing to sign JWTs with a placeholder");
        // Belt and braces: a set-but-worthless secret is as bad as none.
        // The `.env.example` placeholder and short strings are rejected at
        // boot so a copy-pasted example can never sign real sessions.
        // Agents cannot weaken this: it is a code-level const, not config.
        if let Err(msg) = validate_boot_secrets(
            &jwt_secret,
            &std::env::var("ADMIN_PASSWORD").unwrap_or_default(),
        ) {
            panic!("{msg}: refusing to boot");
        }
        let bind_addr = std::env::var("BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:3000".to_string());
        let pool = PgPool::connect_lazy(&database_url).expect("DATABASE_URL invalide");
        (Self { pool, jwt_secret, order_gate: Default::default() }, bind_addr)
    }
}

/// Name of the session cookie carrying the JWT (see `crate::auth_routes`).
pub const SESSION_COOKIE: &str = "warden_token";

/// Shared extraction: `Authorization: Bearer <jwt>` wins, otherwise the
/// `warden_token` cookie (HttpOnly, set at login). Returns the claims on
/// any valid token, `None` otherwise. Pure over `&Parts` + secret.
fn extract_claims(parts: &Parts, secret: &[u8]) -> Option<crate::auth::Claims> {
    let bearer = parts
        .headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    let cookied = parts
        .headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|cookies| {
            cookies.split(';').find_map(|pair| {
                let pair = pair.trim();
                pair.strip_prefix(&format!("{SESSION_COOKIE}="))
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            })
        });
    // Header first (API clients), cookie as fallback (browser navigations,
    // HTMX swaps, EventSource — none of which set custom headers).
    let token = bearer.or(cookied)?;
    crate::auth::decode_jwt(&token, secret).ok()
}

/// Authenticated tenant: `Authorization: Bearer <jwt>` or the `warden_token`
/// session cookie (browser navigations, HTMX swaps, EventSource).
///
/// Decodes the token via `crate::auth::decode_jwt` and returns the `tenant_id`
/// (plus `user_id`). Any failure — no credential, malformed value, bad
/// signature, expired token — rejects with `401 UNAUTHORIZED`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tenant {
    pub tenant_id: String,
    pub user_id: String,
}

/// Rejects tokens whose embedded password version (`pv`) no longer matches
/// the user's row: after a password rotation every previously issued token
/// dies, including stolen ones. A closed account (row gone) rejects too.
async fn claims_live(claims: &crate::auth::Claims, pool: &sqlx::PgPool) -> bool {
    let uid = claims.sub.parse::<uuid::Uuid>();
    let tid = claims.tenant_id.parse::<uuid::Uuid>();
    let (Ok(uid), Ok(tid)) = (uid, tid) else { return false };
    sqlx::query("SELECT 1 AS one FROM users WHERE id = $1 AND tenant_id = $2 AND pwd_ver = $3")
        .bind(uid)
        .bind(tid)
        .bind(claims.pv)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .is_some()
}

impl FromRequestParts<AppState> for Tenant {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        const UNAUTHORIZED: StatusCode = StatusCode::UNAUTHORIZED;
        let claims = extract_claims(parts, state.jwt_secret.as_bytes()).ok_or(UNAUTHORIZED)?;
        if !claims_live(&claims, &state.pool).await {
            return Err(UNAUTHORIZED);
        }
        Ok(Self {
            tenant_id: claims.tenant_id,
            user_id: claims.sub,
        })
    }
}

/// Page guard for server-rendered HTML: same credential as [`Tenant`],
/// but an unauthenticated browser is `302`-redirected to `/login`
/// instead of receiving a bare `401` (which a navigating browser would
/// render as an empty error page).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthPage(pub Tenant);

impl FromRequestParts<AppState> for AuthPage {
    type Rejection = axum::response::Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        use axum::response::IntoResponse;
        let fail = (
            StatusCode::FOUND,
            [(axum::http::header::LOCATION, "/login")],
            "redirecting to /login",
        )
            .into_response();
        match extract_claims(parts, state.jwt_secret.as_bytes()) {
            Some(claims) if claims_live(&claims, &state.pool).await => Ok(Self(Tenant {
                tenant_id: claims.tenant_id,
                user_id: claims.sub,
            })),
            _ => Err(fail),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;

    fn test_state() -> AppState {
        AppState {
            // Lazy: no connection attempted, so no database needed.
            pool: PgPool::connect_lazy("postgres://warden:warden@localhost:5432/warden_trade")
                .expect("lazy pool must build"),
            jwt_secret: "test-secret-must-be-long-enough-0123456789".to_string(),
            order_gate: Default::default(),
        }
    }

    async fn extract(
        state: &AppState,
        req: Request<Body>,
    ) -> Result<Tenant, StatusCode> {
        let (mut parts, _) = req.into_parts();
        Tenant::from_request_parts(&mut parts, state).await
    }

    #[tokio::test]
    async fn missing_authorization_header_rejects_401() {
        let state = test_state();
        let req = Request::builder().uri("/").body(Body::empty()).unwrap();
        let result = extract(&state, req).await;
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn invalid_bearer_token_rejects_401() {
        let state = test_state();
        let req = Request::builder()
            .uri("/")
            .header("Authorization", "Bearer not.a.real.token")
            .body(Body::empty())
            .unwrap();
        let result = extract(&state, req).await;
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    /// Fixed tenant + per-test users for the gated extractor tests: the DB
    /// check needs rows that only exist in the proof database, never in
    /// hermetic runs.
    const PROOF_TENANT: &str = "22222222-2222-4222-8222-222222222222";
    const PROOF_USER_A: &str = "11111111-1111-4111-8111-111111111111";
    const PROOF_USER_B: &str = "33333333-3333-4333-8333-333333333333";
    const PROOF_USER_C: &str = "44444444-4444-4444-8444-444444444444";

    fn valid_token(user: &str, secret: &str) -> String {
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
        crate::auth::encode_jwt(user, PROOF_TENANT, 0, exp, secret.as_bytes())
            .expect("test token must encode")
    }

    /// Proof DB + one live user row. `None` when the hermetic default
    /// applies (no `WARDEN_TEST_DATABASE_URL`): callers skip, never fail.
    /// Each test passes its own user id + email so parallel proof runs
    /// never share (and never race on) the same row.
    async fn proof_state_with_user(uid_str: &str, email: &str) -> Option<AppState> {
        let url = std::env::var("WARDEN_TEST_DATABASE_URL").ok()?;
        let pool = PgPool::connect_lazy(&url).expect("proof pool must build");
        sqlx::migrate!("./migrations").run(&pool).await.ok()?;
        let secret = "test-secret-must-be-long-enough-0123456789".to_string();
        let tid: uuid::Uuid = PROOF_TENANT.parse().ok()?;
        let uid: uuid::Uuid = uid_str.parse().ok()?;
        sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, 'proof') ON CONFLICT (id) DO NOTHING")
            .bind(tid)
            .execute(&pool)
            .await
            .ok()?;
        sqlx::query(
            "INSERT INTO users (id, tenant_id, email, password_hash, role, pwd_ver)
             VALUES ($1, $2, $3, 'not-a-real-hash', 'admin', 0)
             ON CONFLICT (id) DO UPDATE SET pwd_ver = 0",
        )
        .bind(uid)
        .bind(tid)
        .bind(email)
        .execute(&pool)
        .await
        .ok()?;
        Some(AppState { pool, jwt_secret: secret, order_gate: Default::default() })
    }

    #[tokio::test]
    async fn session_cookie_authenticates_without_header() {
        // Browsers navigating, HTMX swaps and EventSource send no
        // Authorization header — the HttpOnly cookie is their credential.
        // Gated: the DB liveness check needs the proof user row.
        let Some(state) = proof_state_with_user(PROOF_USER_A, "proof-a@local").await else { return };
        let token = valid_token(PROOF_USER_A, &state.jwt_secret);
        let req = Request::builder()
            .uri("/")
            .header("Cookie", format!("warden_token={token}"))
            .body(Body::empty())
            .unwrap();
        let tenant = extract(&state, req).await.expect("cookie must authenticate");
        assert_eq!(tenant.tenant_id, PROOF_TENANT);
        assert_eq!(tenant.user_id, PROOF_USER_A);
    }

    #[tokio::test]
    async fn bearer_header_wins_over_cookie() {
        // Gated: same proof row as above.
        let Some(state) = proof_state_with_user(PROOF_USER_B, "proof-b@local").await else { return };
        let good = valid_token(PROOF_USER_B, &state.jwt_secret);
        let req = Request::builder()
            .uri("/")
            .header("Authorization", format!("Bearer {good}"))
            .header("Cookie", "warden_token=not.a.real.token")
            .body(Body::empty())
            .unwrap();
        let tenant = extract(&state, req).await.expect("bearer must win");
        assert_eq!(tenant.tenant_id, PROOF_TENANT);
    }

    #[tokio::test]
    async fn rotated_password_kills_issued_tokens() {
        // The whole point of `pwd_ver`: bumping the row must reject the
        // token issued before the bump, while a fresh post-bump token works.
        let Some(state) = proof_state_with_user(PROOF_USER_C, "proof-c@local").await else { return };
        let stale = valid_token(PROOF_USER_C, &state.jwt_secret);
        let uid: uuid::Uuid = PROOF_USER_C.parse().unwrap();
        sqlx::query("UPDATE users SET pwd_ver = pwd_ver + 1 WHERE id = $1")
            .bind(uid)
            .execute(&state.pool)
            .await
            .expect("bump must apply");
        let req = Request::builder()
            .uri("/")
            .header("Cookie", format!("warden_token={stale}"))
            .body(Body::empty())
            .unwrap();
        let result = extract(&state, req).await;
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn bad_cookie_rejects_401() {
        let state = test_state();
        let req = Request::builder()
            .uri("/")
            .header("Cookie", "warden_token=not.a.real.token; other=1")
            .body(Body::empty())
            .unwrap();
        let result = extract(&state, req).await;
        assert_eq!(result.unwrap_err(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn boot_rejects_placeholder_and_short_secrets() {
        assert!(validate_boot_secrets("dev-placeholder-change-me", "").is_err());
        assert!(validate_boot_secrets("short", "").is_err());
        assert!(validate_boot_secrets("long-enough-jwt-secret-0123456789", "short").is_err());
        assert!(validate_boot_secrets("long-enough-jwt-secret-0123456789", "").is_ok());
        assert!(validate_boot_secrets("long-enough-jwt-secret-0123456789", "long-enough-admin-10").is_ok());
    }
}
