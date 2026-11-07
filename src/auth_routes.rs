//! Auth routes (paper trading, Postgres-backed).
//!
//! `POST /api/auth/login` accepts `{email, password}` and returns a JWT.
//! Solo-admin first: the `solo-admin` tenant and admin user are
//! auto-provisioned on first login using `ADMIN_EMAIL` / `ADMIN_PASSWORD`
//! env (dev defaults, must be changed in production).

use askama::Template;
use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

fn admin_email() -> String {
    std::env::var("ADMIN_EMAIL").unwrap_or_else(|_| "admin@local".to_string())
}

fn admin_password() -> String {
    std::env::var("ADMIN_PASSWORD")
        .expect("ADMIN_PASSWORD must be set: refusing to auto-provision admin with a public default")
}

/// Incoming login body.
#[derive(Debug, Clone, Deserialize)]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
}

/// Outgoing login response carrying the JWT.
#[derive(Debug, Clone, Serialize)]
pub struct LoginResponse {
    pub token: String,
}

/// Authenticate and issue a JWT (HS256, 24h expiry).
///
/// Auto-provisions the `solo-admin` tenant and admin user on first use.
/// Wrong credentials become `401`; DB failures become `503`.
///
/// The JWT is returned BOTH as JSON (API clients) and as an HttpOnly
/// `warden_token` cookie (browser: navigations, HTMX, EventSource send
/// no Authorization header). `Secure` because the site is HTTPS-only;
/// `SameSite=Lax` so top-level navigation to `/` after login carries it.
pub async fn login_handler(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<(axum::http::HeaderMap, Json<LoginResponse>), (StatusCode, String)> {
    let email = req.email.trim().to_ascii_lowercase();
    if email.is_empty() || req.password.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "email and password required".to_string()));
    }

    let tenant_id: uuid::Uuid = match sqlx::query("SELECT id FROM tenants ORDER BY created_at ASC LIMIT 1")
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")))?
    {
        Some(row) => {
            use sqlx::Row;
            row.try_get("id")
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("tenant: {e}")))?
        }
        None => {
            let row = sqlx::query("INSERT INTO tenants (name) VALUES ('solo-admin') RETURNING id")
                .fetch_one(&state.pool)
                .await
                .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")))?;
            use sqlx::Row;
            row.try_get("id")
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("tenant: {e}")))?
        }
    };

    let admin_email_norm = admin_email().to_ascii_lowercase();
    let user_row = sqlx::query(
        "SELECT id, password_hash FROM users WHERE tenant_id = $1 AND email = $2",
    )
    .bind(tenant_id)
    .bind(&email)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")))?;

    let user_id: uuid::Uuid = match user_row {
        Some(row) => {
            use sqlx::Row;
            let hash: String = row
                .try_get("password_hash")
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("user: {e}")))?;
            if !crate::auth::verify_password(&hash, &req.password) {
                return Err((StatusCode::UNAUTHORIZED, "invalid credentials".to_string()));
            }
            row.try_get("id")
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("user: {e}")))?
        }
        None => {
            // Auto-provision only the configured admin email AND the configured
            // admin password; anything else is 401. Without this check, anyone
            // calling login first with the (public-shaped) admin email could
            // claim the admin account with a password of their choice.
            if email != admin_email_norm || req.password != admin_password() {
                // Same error either way: never reveal whether the email exists.
                return Err((StatusCode::UNAUTHORIZED, "invalid credentials".to_string()));
            }
            let hash = crate::auth::hash_password(&req.password)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("hash: {e}")))?;
            let row = sqlx::query(
                "INSERT INTO users (tenant_id, email, password_hash, role) VALUES ($1, $2, $3, 'admin') RETURNING id",
            )
            .bind(tenant_id)
            .bind(&email)
            .bind(&hash)
            .fetch_one(&state.pool)
            .await
            .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")))?;
            use sqlx::Row;
            row.try_get("id")
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("user: {e}")))?
        }
    };

    let exp = (chrono::Utc::now() + chrono::Duration::hours(24)).timestamp() as usize;
    let token = crate::auth::encode_jwt(
        &user_id.to_string(),
        &tenant_id.to_string(),
        exp,
        state.jwt_secret.as_bytes(),
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("jwt: {e}")))?;
    let mut headers = axum::http::HeaderMap::new();
    let cookie = format!(
        "{}={token}; Path=/; Max-Age=86400; HttpOnly; Secure; SameSite=Lax",
        crate::state::SESSION_COOKIE
    );
    if let Ok(value) = cookie.parse() {
        headers.insert(axum::http::header::SET_COOKIE, value);
    }
    Ok((headers, Json(LoginResponse { token })))
}

/// Server-rendered solo-admin login page.
///
/// Same-origin HTML shell; the form POSTs JSON to `/api/auth/login` and
/// the server sets the HttpOnly `warden_token` session cookie (plus the
/// JSON token for API clients). No secret is embedded in the page and
/// the browser stores nothing in `localStorage`.
#[derive(askama::Template)]
#[template(path = "login.html")]
pub struct LoginTemplate;

/// `GET /login` renders the sign-in form.
pub async fn login_page_handler() -> axum::response::Html<String> {
    axum::response::Html(LoginTemplate.render().unwrap_or_default())
}

#[cfg(test)]
mod page_tests {
    use super::*;

    #[tokio::test]
    async fn login_page_renders_form_without_db() {
        let axum::response::Html(html) = login_page_handler().await;
        assert!(html.contains("<form"), "login page must render a form, got {html:?}");
        assert!(html.contains("/api/auth/login"), "form must target the login API");
        assert!(html.contains("window.location.href = \"/\""), "login must redirect to the dashboard");
        assert!(!html.contains("localStorage"), "session must live in the HttpOnly cookie, not localStorage");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_request_parses() {
        let req: LoginRequest =
            serde_json::from_str(r#"{"email":"admin@local","password":"secret"}"#)
                .expect("login parses");
        assert_eq!(req.email, "admin@local");
    }

    #[test]
    fn empty_password_rejected_shape() {
        let req = LoginRequest { email: "a@b.c".to_string(), password: String::new() };
        assert!(req.password.is_empty());
    }
}
