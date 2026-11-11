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

/// Internal failure mapped to a generic message: the detail goes to the
/// server log (tracing), never to the client. Auth endpoints must not leak
/// schema, column, driver or template internals in error bodies.
fn fail(status: StatusCode, what: &str, e: impl std::fmt::Display) -> (StatusCode, String) {
    tracing::warn!("auth {what} degraded: {e}");
    (status, "service unavailable".to_string())
}

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
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?
    {
        Some(row) => {
            use sqlx::Row;
            row.try_get("id")
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "tenant", e))?
        }
        None => {
            let row = sqlx::query("INSERT INTO tenants (name) VALUES ('solo-admin') RETURNING id")
                .fetch_one(&state.pool)
                .await
                .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
            use sqlx::Row;
            row.try_get("id")
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "tenant", e))?
        }
    };

    let admin_email_norm = admin_email().to_ascii_lowercase();
    let user_row = sqlx::query(
        "SELECT id, password_hash, pwd_ver FROM users WHERE tenant_id = $1 AND email = $2",
    )
    .bind(tenant_id)
    .bind(&email)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;

    let (user_id, pwd_ver): (uuid::Uuid, i32) = match user_row {
        Some(row) => {
            use sqlx::Row;
            let hash: String = row
                .try_get("password_hash")
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
            if !crate::auth::verify_password(&hash, &req.password) {
                return Err((StatusCode::UNAUTHORIZED, "invalid credentials".to_string()));
            }
            let id: uuid::Uuid = row
                .try_get("id")
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
            let pv: i32 = row
                .try_get("pwd_ver")
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
            (id, pv)
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
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "hash", e))?;
            let row = sqlx::query(
                "INSERT INTO users (tenant_id, email, password_hash, role) VALUES ($1, $2, $3, 'admin') RETURNING id",
            )
            .bind(tenant_id)
            .bind(&email)
            .bind(&hash)
            .fetch_one(&state.pool)
            .await
            .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
            use sqlx::Row;
            let id: uuid::Uuid = row
                .try_get("id")
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
            (id, 0)
        }
    };

    let exp = (chrono::Utc::now() + chrono::Duration::hours(24)).timestamp() as usize;
    let token = crate::auth::encode_jwt(
        &user_id.to_string(),
        &tenant_id.to_string(),
        pwd_ver,
        exp,
        state.jwt_secret.as_bytes(),
    )
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "jwt", e))?;
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

/// `POST /api/auth/logout`: expire the `warden_token` session cookie.
///
/// Deliberately unauthenticated: the holder of an expired token must
/// still be able to sign out. Returns `200 {"ok": true}` with the
/// expired cookie.
pub async fn logout_handler() -> (axum::http::HeaderMap, Json<serde_json::Value>) {
    let mut headers = axum::http::HeaderMap::new();
    let cookie = format!(
        "{}=; Path=/; Max-Age=0; HttpOnly; Secure; SameSite=Lax",
        crate::state::SESSION_COOKIE
    );
    if let Ok(value) = cookie.parse() {
        headers.insert(axum::http::header::SET_COOKIE, value);
    }
    (headers, Json(serde_json::json!({"ok": true})))
}

/// Kill-switch for public registration. Open signup is `SIGNUP_ENABLED=1`
/// ONLY: by default the route answers 404, because a new account lands in
/// the solo tenant today and tenant isolation is the precondition for a
/// safe public signup (one tenant per account, scheduler iterating over
/// tenants). Agents cannot flip this: it is process env, not a DB flag.
fn signup_enabled() -> bool {
    matches!(
        std::env::var("SIGNUP_ENABLED").ok().as_deref(),
        Some("1") | Some("true") | Some("yes")
    )
}
/// Minimum length for newly chosen passwords (signup + password change).
/// Login keeps accepting whatever was set before: this bar only ever
/// ratchets new secrets up, and agents cannot touch it (env-free const).
pub const MIN_PASSWORD_LEN: usize = 10;

fn normalize_email(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

/// Shape check only (`local@domain.tld`): delivery is never attempted,
/// so deliverability heuristics would be theater. No regex dep.
fn valid_email(email: &str) -> bool {
    let mut parts = email.split('@');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        }
        _ => false,
    }
}

/// First tenant by age, creating `solo-admin` when the DB is empty.
/// Same rule as the login auto-provision path, factored for reuse.
async fn solo_tenant_id(
    pool: &sqlx::PgPool,
) -> Result<uuid::Uuid, (StatusCode, String)> {
    match sqlx::query("SELECT id FROM tenants ORDER BY created_at ASC LIMIT 1")
        .fetch_optional(pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?
    {
        Some(row) => {
            use sqlx::Row;
            row.try_get("id")
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "tenant", e))
        }
        None => {
            let row = sqlx::query("INSERT INTO tenants (name) VALUES ('solo-admin') RETURNING id")
                .fetch_one(pool)
                .await
                .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
            use sqlx::Row;
            row.try_get("id")
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "tenant", e))
        }
    }
}

/// Issue the dual session (JSON token + HttpOnly cookie). Same shape as
/// login: API clients read the JSON, browsers ride the cookie.
fn issue_session(
    state: &AppState,
    user_id: &uuid::Uuid,
    tenant_id: &uuid::Uuid,
    pwd_ver: i32,
) -> Result<(axum::http::HeaderMap, String), (StatusCode, String)> {
    let exp = (chrono::Utc::now() + chrono::Duration::hours(24)).timestamp() as usize;
    let token = crate::auth::encode_jwt(
        &user_id.to_string(),
        &tenant_id.to_string(),
        pwd_ver,
        exp,
        state.jwt_secret.as_bytes(),
    )
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "jwt", e))?;
    let mut headers = axum::http::HeaderMap::new();
    let cookie = format!(
        "{}={token}; Path=/; Max-Age=86400; HttpOnly; Secure; SameSite=Lax",
        crate::state::SESSION_COOKIE
    );
    if let Ok(value) = cookie.parse() {
        headers.insert(axum::http::header::SET_COOKIE, value);
    }
    Ok((headers, token))
}

/// Incoming signup body.
#[derive(Debug, Clone, Deserialize)]
pub struct SignupRequest {
    pub email: String,
    pub password: String,
}

/// `POST /api/auth/signup`: create a `member` account in the solo tenant.
///
/// Anyone with the URL can register — acceptable because the product is
/// paper-only (no real funds, no withdrawals, nothing to steal). The
/// response logs the new user in immediately (same dual session as login).
/// Duplicate email becomes `409`; weak credentials become `400`.
pub async fn signup_handler(
    State(state): State<AppState>,
    Json(req): Json<SignupRequest>,
) -> Result<(StatusCode, axum::http::HeaderMap, Json<LoginResponse>), (StatusCode, String)> {
    if !signup_enabled() {
        return Err((StatusCode::NOT_FOUND, "not found".to_string()));
    }
    let email = normalize_email(&req.email);
    if !valid_email(&email) {
        return Err((StatusCode::BAD_REQUEST, "valid email required".to_string()));
    }
    if req.password.len() < MIN_PASSWORD_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("password too short (min {MIN_PASSWORD_LEN} characters)"),
        ));
    }
    let tenant_id = solo_tenant_id(&state.pool).await?;
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM users WHERE tenant_id = $1 AND email = $2)",
    )
    .bind(tenant_id)
    .bind(&email)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    if taken {
        return Err((StatusCode::CONFLICT, "email already registered".to_string()));
    }
    let hash = crate::auth::hash_password(&req.password)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "hash", e))?;
    let row = sqlx::query(
        "INSERT INTO users (tenant_id, email, password_hash, role) VALUES ($1, $2, $3, 'member') RETURNING id",
    )
    .bind(tenant_id)
    .bind(&email)
    .bind(&hash)
    .fetch_one(&state.pool)
    .await
    .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    use sqlx::Row;
    let user_id: uuid::Uuid = row
        .try_get("id")
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    // Fresh row: nobody rotated yet, so the session starts at version 0.
    let (headers, token) = issue_session(&state, &user_id, &tenant_id, 0)?;
    Ok((StatusCode::CREATED, headers, Json(LoginResponse { token })))
}

/// Server-rendered signup page (same SaaS card as login).
#[derive(askama::Template)]
#[template(path = "signup.html")]
pub struct SignupTemplate;

/// `GET /signup` renders the registration form. Public, like `/login`.
pub async fn signup_page_handler(
) -> Result<axum::response::Html<String>, (StatusCode, String)> {
    if !signup_enabled() {
        return Err((StatusCode::NOT_FOUND, "not found".to_string()));
    }
    Ok(axum::response::Html(SignupTemplate.render().unwrap_or_default()))
}

/// Account data shown on the settings page. Never a hash, never a token.
#[derive(askama::Template)]
#[template(path = "account.html")]
pub struct AccountTemplate {
    pub email: String,
    pub role: String,
    pub member_since: String,
}

/// `GET /account`: settings page for the signed-in user.
pub async fn account_page_handler(
    State(state): State<AppState>,
    crate::state::AuthPage(tenant): crate::state::AuthPage,
) -> Result<axum::response::Html<String>, (StatusCode, String)> {
    let user_id: uuid::Uuid = tenant
        .user_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let tenant_id: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let row = sqlx::query(
        "SELECT email, role, created_at FROM users WHERE id = $1 AND tenant_id = $2",
    )
    .bind(user_id)
    .bind(tenant_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    let Some(row) = row else {
        return Err((StatusCode::NOT_FOUND, "account not found".to_string()));
    };
    use sqlx::Row;
    let email: String = row
        .try_get("email")
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    let role: String = row
        .try_get("role")
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    let created: chrono::DateTime<chrono::Utc> = row
        .try_get("created_at")
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    let page = AccountTemplate {
        email,
        role,
        member_since: created.format("%Y-%m-%d").to_string(),
    };
    Ok(axum::response::Html(
        page.render()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("template: {e}")))?,
    ))
}

/// Incoming password-change body.
#[derive(Debug, Clone, Deserialize)]
pub struct ChangePasswordRequest {
    pub current_password: String,
    pub new_password: String,
}

/// `POST /api/auth/change-password`: rotate the caller's own password.
///
/// Verifies the current password first (stolen-session window stays shut),
/// then enforces the new-secret bar. Reusing the current password is a
/// `400`: rotation must actually rotate.
pub async fn change_password_handler(
    State(state): State<AppState>,
    tenant: crate::state::Tenant,
    Json(req): Json<ChangePasswordRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if req.new_password.len() < MIN_PASSWORD_LEN {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("password too short (min {MIN_PASSWORD_LEN} characters)"),
        ));
    }
    let user_id: uuid::Uuid = tenant
        .user_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let tenant_id: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let row = sqlx::query("SELECT password_hash FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(user_id)
        .bind(tenant_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    let Some(row) = row else {
        return Err((StatusCode::NOT_FOUND, "account not found".to_string()));
    };
    use sqlx::Row;
    let hash: String = row
        .try_get("password_hash")
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    if !crate::auth::verify_password(&hash, &req.current_password) {
        return Err((StatusCode::UNAUTHORIZED, "current password is wrong".to_string()));
    }
    if crate::auth::verify_password(&hash, &req.new_password) {
        return Err((
            StatusCode::BAD_REQUEST,
            "new password must differ from the current one".to_string(),
        ));
    }
    let new_hash = crate::auth::hash_password(&req.new_password)
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "hash", e))?;
    // The version bump invalidates every previously issued token: their
    // embedded `pv` no longer matches the row, so extractors reject them.
    sqlx::query("UPDATE users SET password_hash = $1, pwd_ver = pwd_ver + 1 WHERE id = $2 AND tenant_id = $3")
        .bind(&new_hash)
        .bind(user_id)
        .bind(tenant_id)
        .execute(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Incoming account-deletion body: the password re-confirms identity so a
/// stolen session alone cannot erase the account.
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteAccountRequest {
    pub password: String,
}

/// `DELETE /api/auth/account`: erase the caller's own account.
///
/// Verifies the password first, then a single `DELETE`: verification
/// tokens, reset tokens and backup codes cascade on the FK, and every
/// issued JWT dies on next use because the user row (and its `pwd_ver`)
/// is gone. Tenant-scoped trading history is the operator's own
/// bookkeeping and is retained.
pub async fn delete_account_handler(
    State(state): State<AppState>,
    tenant: crate::state::Tenant,
    Json(req): Json<DeleteAccountRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let user_id: uuid::Uuid = tenant
        .user_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let tenant_id: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let row = sqlx::query("SELECT password_hash FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(user_id)
        .bind(tenant_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    let Some(row) = row else {
        return Err((StatusCode::NOT_FOUND, "account not found".to_string()));
    };
    use sqlx::Row;
    let hash: String = row
        .try_get("password_hash")
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    if !crate::auth::verify_password(&hash, &req.password) {
        return Err((StatusCode::UNAUTHORIZED, "current password is wrong".to_string()));
    }
    sqlx::query("DELETE FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(user_id)
        .bind(tenant_id)
        .execute(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    Ok(Json(serde_json::json!({ "deleted": true })))
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
    async fn logout_expires_session_cookie() {
        let (headers, Json(body)) = logout_handler().await;
        assert_eq!(body, serde_json::json!({"ok": true}));
        let set_cookie = headers
            .get(axum::http::header::SET_COOKIE)
            .expect("logout must expire the cookie")
            .to_str()
            .expect("cookie must be ASCII")
            .to_string();
        assert!(set_cookie.starts_with("warden_token=;"), "must clear the value, got {set_cookie:?}");
        assert!(set_cookie.contains("Max-Age=0"), "must expire immediately, got {set_cookie:?}");
        assert!(set_cookie.contains("HttpOnly"), "must stay JS-invisible, got {set_cookie:?}");
    }

    #[tokio::test]
    async fn login_page_renders_form_without_db() {
        let axum::response::Html(html) = login_page_handler().await;
        assert!(html.contains("<form"), "login page must render a form, got {html:?}");
        assert!(html.contains("/api/auth/login"), "form must target the login API");
        assert!(html.contains("window.location.href = \"/\""), "login must redirect to the dashboard");
        assert!(!html.contains("localStorage"), "session must live in the HttpOnly cookie, not localStorage");
        assert!(html.contains("password-eye"), "login must offer a show/hide password toggle");
        assert!(!html.contains("/signup"), "login must not advertise signup while it is closed");
    }

    #[tokio::test]
    async fn signup_page_renders_form_without_db() {
        let _lock = signup_test_lock();
        let prev = std::env::var("SIGNUP_ENABLED").ok();
        std::env::set_var("SIGNUP_ENABLED", "1");
        let res = signup_page_handler().await;
        restore_signup_env(prev);
        let axum::response::Html(html) = res.expect("signup page renders when enabled");
        assert!(html.contains("<form"), "signup page must render a form");
        assert!(html.contains("/api/auth/signup"), "form must target the signup API");
        assert!(html.contains("password-eye"), "signup must offer a show/hide password toggle");
        assert!(html.contains("/login"), "signup must link back to sign-in");
        assert!(!html.contains("localStorage"), "session must live in the HttpOnly cookie, not localStorage");
    }

    #[tokio::test]
    async fn signup_closed_by_default() {
        let _lock = signup_test_lock();
        let prev = std::env::var("SIGNUP_ENABLED").ok();
        std::env::remove_var("SIGNUP_ENABLED");
        let page = signup_page_handler().await;
        assert!(
            matches!(page, Err((StatusCode::NOT_FOUND, _))),
            "signup page must 404 unless SIGNUP_ENABLED=1"
        );
        restore_signup_env(prev);
    }

    /// Serializes the env-flag tests: `SIGNUP_ENABLED` is process-global.
    pub(super) fn signup_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        // Poison-tolerant: a panicking holder (e.g. a failed signup attempt)
        // must not cascade into every other serialized test.
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub(super) fn restore_signup_env(prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var("SIGNUP_ENABLED", v),
            None => std::env::remove_var("SIGNUP_ENABLED"),
        }
    }

    #[tokio::test]
    async fn account_template_renders_profile_without_secrets() {
        let page = AccountTemplate {
            email: "trader@example.com".to_string(),
            role: "member".to_string(),
            member_since: "2026-11-09".to_string(),
        };
        let html = page.render().expect("account renders");
        assert!(html.contains("trader@example.com"), "account must show the email");
        assert!(html.contains("member"), "account must show the role");
        assert!(html.contains("2026-11-09"), "account must show member-since");
        assert!(html.contains("/api/auth/change-password"), "account must wire the password form");
        assert!(html.contains("password-eye"), "password fields must offer show/hide toggles");
        assert!(!html.contains("password_hash"), "template must never touch hashes, got {html:?}");
        assert!(!html.contains("localStorage"), "no client-side secret storage");
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

    #[test]
    fn email_shape_check() {
        assert!(valid_email("trader@example.com"));
        assert!(valid_email("a@b.cd"));
        assert!(!valid_email("no-at-sign"));
        assert!(!valid_email("a@b"));
        assert!(!valid_email("@b.com"));
        assert!(!valid_email("a@.com"));
        assert!(!valid_email("a@b."));
        assert!(!valid_email("a@b@c.com"));
        assert!(!valid_email(""));
    }

    #[test]
    fn password_bar_is_ten() {
        assert_eq!(MIN_PASSWORD_LEN, 10);
    }
}

/// End-to-end account lifecycle against a real Postgres, gated on
/// `WARDEN_TEST_DATABASE_URL` like every other DB proof in this repo:
/// signup -> login -> wrong-password change rejected -> change -> new
/// login works, old password dead.
#[cfg(test)]
mod account_flow_tests {
    use super::*;
    use axum::extract::State;

    fn proof_state() -> Option<AppState> {
        let url = std::env::var("WARDEN_TEST_DATABASE_URL").ok()?;
        let pool = sqlx::PgPool::connect_lazy(&url).ok()?;
        Some(AppState { pool, jwt_secret: "proof-secret-for-account-flow".to_string(), order_gate: Default::default() })
    }

    #[tokio::test]
    async fn signup_login_change_password_roundtrip() {
        let _lock = super::page_tests::signup_test_lock();
        let prev = std::env::var("SIGNUP_ENABLED").ok();
        std::env::set_var("SIGNUP_ENABLED", "1");
        let Some(state) = proof_state() else {
            super::page_tests::restore_signup_env(prev);
            return;
        };
        let email = format!("flow{}@example.com", chrono::Utc::now().timestamp_millis());

        // Signup creates the account and logs in immediately (201 + cookie).
        let (status, headers, Json(body)) = signup_handler(
            State(state.clone()),
            Json(SignupRequest { email: email.clone(), password: "correct-horse-10".to_string() }),
        )
        .await
        .expect("signup must succeed");
        assert_eq!(status, StatusCode::CREATED);
        assert!(!body.token.is_empty());
        let cookie = headers
            .get(axum::http::header::SET_COOKIE)
            .expect("signup must set the session cookie")
            .to_str()
            .expect("cookie ASCII")
            .to_string();
        assert!(cookie.starts_with("warden_token="), "session cookie, got {cookie:?}");
        assert!(cookie.contains("HttpOnly"), "cookie must stay JS-invisible");

        // Duplicate signup is a clean 409.
        let dup = signup_handler(
            State(state.clone()),
            Json(SignupRequest { email: email.clone(), password: "another-long-10".to_string() }),
        )
        .await;
        assert!(matches!(dup, Err((StatusCode::CONFLICT, _))), "duplicate must 409, got {dup:?}");

        // Short password is a clean 400.
        let weak = signup_handler(
            State(state.clone()),
            Json(SignupRequest { email: "weak@example.com".to_string(), password: "short".to_string() }),
        )
        .await;
        assert!(matches!(weak, Err((StatusCode::BAD_REQUEST, _))), "weak must 400, got {weak:?}");

        // Login works with the fresh credentials.
        let (_, Json(login)) = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: "correct-horse-10".to_string() }),
        )
        .await
        .expect("login with fresh credentials must succeed");
        assert!(!login.token.is_empty());
        let claims = crate::auth::decode_jwt(&login.token, state.jwt_secret.as_bytes())
            .expect("token must decode");
        let tenant = crate::state::Tenant { tenant_id: claims.tenant_id, user_id: claims.sub };

        // Wrong current password cannot rotate.
        let bad = change_password_handler(
            State(state.clone()),
            tenant.clone(),
            Json(ChangePasswordRequest {
                current_password: "not-the-password-10".to_string(),
                new_password: "brand-new-horse-10".to_string(),
            }),
        )
        .await;
        assert!(matches!(bad, Err((StatusCode::UNAUTHORIZED, _))), "wrong current must 401, got {bad:?}");

        // Rotation works.
        let Json(ok) = change_password_handler(
            State(state.clone()),
            tenant.clone(),
            Json(ChangePasswordRequest {
                current_password: "correct-horse-10".to_string(),
                new_password: "brand-new-horse-10".to_string(),
            }),
        )
        .await
        .expect("rotation must succeed");
        assert_eq!(ok, serde_json::json!({ "ok": true }));

        // New password logs in, old password is dead.
        let _ = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: "brand-new-horse-10".to_string() }),
        )
        .await
        .expect("login with rotated password must succeed");
        let stale = login_handler(
            State(state.clone()),
            Json(LoginRequest { email, password: "correct-horse-10".to_string() }),
        )
        .await;
        assert!(matches!(stale, Err((StatusCode::UNAUTHORIZED, _))), "old password must die, got {stale:?}");

        // The pre-rotation token is dead too: its embedded `pv` no longer
        // matches the bumped row, so the extractor rejects it.
        use axum::body::Body;
        use axum::extract::FromRequestParts;
        let req = axum::http::Request::builder()
            .uri("/")
            .header("Cookie", format!("warden_token={}", login.token))
            .body(Body::empty())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let rejected = crate::state::Tenant::from_request_parts(&mut parts, &state).await;
        assert!(rejected.is_err(), "stale token must not authenticate after rotation");
        super::page_tests::restore_signup_env(prev);
    }

    #[tokio::test]
    async fn delete_account_purges_and_kills_session() {
        let _lock = super::page_tests::signup_test_lock();
        let prev = std::env::var("SIGNUP_ENABLED").ok();
        std::env::set_var("SIGNUP_ENABLED", "1");
        let Some(state) = proof_state() else {
            super::page_tests::restore_signup_env(prev);
            return;
        };
        let email = format!("gone{}@example.com", chrono::Utc::now().timestamp_millis());

        let (_, _, Json(created)) = signup_handler(
            State(state.clone()),
            Json(SignupRequest { email: email.clone(), password: "correct-horse-10".to_string() }),
        )
        .await
        .expect("signup must succeed");
        let claims = crate::auth::decode_jwt(&created.token, state.jwt_secret.as_bytes())
            .expect("token must decode");
        let tenant = crate::state::Tenant { tenant_id: claims.tenant_id, user_id: claims.sub };

        // Wrong password: 401 and the account survives.
        let bad = delete_account_handler(
            State(state.clone()),
            tenant.clone(),
            Json(DeleteAccountRequest { password: "not-the-password-10".to_string() }),
        )
        .await;
        assert!(matches!(bad, Err((StatusCode::UNAUTHORIZED, _))), "wrong password must 401, got {bad:?}");
        let _ = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: "correct-horse-10".to_string() }),
        )
        .await
        .expect("account must survive a wrong-password delete");

        // Right password: the row is gone.
        let Json(ok) = delete_account_handler(State(state.clone()), tenant.clone(), Json(DeleteAccountRequest {
            password: "correct-horse-10".to_string(),
        }))
        .await
        .expect("delete with password must succeed");
        assert_eq!(ok, serde_json::json!({ "deleted": true }));

        // Login is dead and the old token no longer authenticates.
        let stale = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: "correct-horse-10".to_string() }),
        )
        .await;
        assert!(matches!(stale, Err((StatusCode::UNAUTHORIZED, _))), "deleted account must not log in, got {stale:?}");
        use axum::body::Body;
        use axum::extract::FromRequestParts;
        let req = axum::http::Request::builder()
            .uri("/")
            .header("Cookie", format!("warden_token={}", created.token))
            .body(Body::empty())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let rejected = crate::state::Tenant::from_request_parts(&mut parts, &state).await;
        assert!(rejected.is_err(), "deleted account token must not authenticate");

        // The address is free again: erasure really purged the row.
        let (status, _, _) = signup_handler(
            State(state.clone()),
            Json(SignupRequest { email, password: "fresh-horse-10!!".to_string() }),
        )
        .await
        .expect("re-signup after delete must succeed");
        assert_eq!(status, StatusCode::CREATED);
        super::page_tests::restore_signup_env(prev);
    }
}
