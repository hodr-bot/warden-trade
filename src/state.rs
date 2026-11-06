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
    /// Build state from the environment, with dev defaults.
    ///
    /// Reads `DATABASE_URL`, `JWT_SECRET` and `BIND_ADDR`.
    /// Returns the state plus the bind address to listen on.
    /// The pool connects lazily: no database traffic happens until the
    /// first query, so the server boots even when Postgres is down
    /// (DB-backed routes then fall back to empty results and log why).
    pub fn from_env() -> (Self, String) {
        let database_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "postgres://warden:warden@localhost:5432/warden_trade".to_string());
        let jwt_secret = std::env::var("JWT_SECRET")
            .unwrap_or_else(|_| "dev-placeholder-change-me".to_string());
        let bind_addr = std::env::var("BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:3000".to_string());
        let pool = PgPool::connect_lazy(&database_url).expect("DATABASE_URL invalide");
        (Self { pool, jwt_secret }, bind_addr)
    }
}

/// Authenticated tenant extracted from the `Authorization: Bearer <jwt>` header.
///
/// Decodes the token via `crate::auth::decode_jwt` and returns the `tenant_id`
/// (plus `user_id`). Any failure — missing header, malformed value, bad
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
        let header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or(UNAUTHORIZED)?;
        let token = header
            .strip_prefix("Bearer ")
            .filter(|t| !t.is_empty())
            .ok_or(UNAUTHORIZED)?;
        let claims =
            crate::auth::decode_jwt(token, state.jwt_secret.as_bytes()).map_err(|_| UNAUTHORIZED)?;
        Ok(Self {
            tenant_id: claims.tenant_id,
            user_id: claims.sub,
        })
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
}
