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
        let bind_addr = std::env::var("BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:3000".to_string());
        let pool = PgPool::connect_lazy(&database_url).expect("DATABASE_URL invalide");
        (Self { pool, jwt_secret }, bind_addr)
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

impl FromRequestParts<AppState> for Tenant {
    type Rejection = StatusCode;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        const UNAUTHORIZED: StatusCode = StatusCode::UNAUTHORIZED;
        let claims = extract_claims(parts, state.jwt_secret.as_bytes()).ok_or(UNAUTHORIZED)?;
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
        match extract_claims(parts, state.jwt_secret.as_bytes()) {
            Some(claims) => Ok(Self(Tenant {
                tenant_id: claims.tenant_id,
                user_id: claims.sub,
            })),
            None => Err((
                StatusCode::FOUND,
                [(axum::http::header::LOCATION, "/login")],
                "redirecting to /login",
            )
                .into_response()),
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

    fn valid_token(secret: &str) -> String {
        let exp = (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp() as usize;
        crate::auth::encode_jwt("user-1", "tenant-1", exp, secret.as_bytes())
            .expect("test token must encode")
    }

    #[tokio::test]
    async fn session_cookie_authenticates_without_header() {
        // Browsers navigating, HTMX swaps and EventSource send no
        // Authorization header — the HttpOnly cookie is their credential.
        let state = test_state();
        let token = valid_token(&state.jwt_secret);
        let req = Request::builder()
            .uri("/")
            .header("Cookie", format!("warden_token={token}"))
            .body(Body::empty())
            .unwrap();
        let tenant = extract(&state, req).await.expect("cookie must authenticate");
        assert_eq!(tenant.tenant_id, "tenant-1");
        assert_eq!(tenant.user_id, "user-1");
    }

    #[tokio::test]
    async fn bearer_header_wins_over_cookie() {
        let state = test_state();
        let good = valid_token(&state.jwt_secret);
        let req = Request::builder()
            .uri("/")
            .header("Authorization", format!("Bearer {good}"))
            .header("Cookie", "warden_token=not.a.real.token")
            .body(Body::empty())
            .unwrap();
        let tenant = extract(&state, req).await.expect("bearer must win");
        assert_eq!(tenant.tenant_id, "tenant-1");
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
}
