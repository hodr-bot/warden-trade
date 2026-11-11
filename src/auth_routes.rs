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
    // Best-effort verification email: a silent mailer must never fail
    // signup. Log-mode keeps the flow testable with zero credentials.
    match issue_verification_token(&state.pool, user_id, 72).await {
        Ok(raw) => {
            let link = format!("{}/verify?token={raw}", public_base_url());
            let body = format!(
                "Welcome to Warden Trade.\n\nConfirm this address to secure your account:\n{link}\n\nThis link expires in 72 hours. If you did not sign up, ignore this message."
            );
            if let Err(e) = crate::mail::Mailer::from_env()
                .send(&email, "Warden Trade: verify your email", &body)
                .await
            {
                tracing::warn!("verification email failed for {email}: {e}");
            }
        }
        Err(e) => tracing::warn!("verification token failed for {email}: {e}"),
    }
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

/// Public origin used in emailed links. Override per environment with
/// `PUBLIC_BASE_URL`; the default is the production console.
fn public_base_url() -> String {
    std::env::var("PUBLIC_BASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| "https://warden.djeel.org".to_string())
}

/// 32 random bytes, URL-safe base64. The raw value goes only into the
/// email body — never into a log, a response, or the database.
fn new_raw_token() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

/// SHA-256 hex of the raw token: the only form ever stored or compared.
fn token_hash(raw: &str) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(raw.as_bytes());
    hasher.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Replace any outstanding verification token, return the fresh raw value.
async fn issue_verification_token(
    pool: &sqlx::PgPool,
    user_id: uuid::Uuid,
    ttl_hours: i64,
) -> Result<String, sqlx::Error> {
    let raw = new_raw_token();
    let hash = token_hash(&raw);
    sqlx::query("DELETE FROM email_verification_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(&*pool)
        .await?;
    sqlx::query(
        "INSERT INTO email_verification_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, now() + $3::interval)",
    )
    .bind(user_id)
    .bind(&hash)
    .bind(format!("{ttl_hours} hours"))
    .execute(&*pool)
    .await?;
    Ok(raw)
}

/// `GET /verify?token=…`: single-use link that stamps `email_verified_at`.
/// Unknown and expired tokens render the same failure page: nothing about
/// the address book leaks through this endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct VerifyQuery {
    pub token: String,
}

#[derive(askama::Template)]
#[template(path = "verify.html")]
pub struct VerifyTemplate {
    pub ok: bool,
}

pub async fn verify_page_handler(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<VerifyQuery>,
) -> axum::response::Html<String> {
    let ok = consume_verification_token(&state.pool, &query.token).await.unwrap_or(false);
    axum::response::Html(VerifyTemplate { ok }.render().unwrap_or_default())
}

async fn consume_verification_token(pool: &sqlx::PgPool, raw: &str) -> Result<bool, sqlx::Error> {
    let hash = token_hash(raw);
    let row = sqlx::query("SELECT user_id, expires_at FROM email_verification_tokens WHERE token_hash = $1")
        .bind(&hash)
        .fetch_optional(&*pool)
        .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    use sqlx::Row;
    let user_id: uuid::Uuid = row.try_get("user_id")?;
    let expires_at: chrono::DateTime<chrono::Utc> = row.try_get("expires_at")?;
    sqlx::query("DELETE FROM email_verification_tokens WHERE token_hash = $1")
        .bind(&hash)
        .execute(&*pool)
        .await?;
    if expires_at < chrono::Utc::now() {
        return Ok(false);
    }
    sqlx::query("UPDATE users SET email_verified_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&*pool)
        .await?;
    Ok(true)
}

/// `POST /api/auth/verify/resend`: fresh link for the signed-in user.
/// Already-verified addresses get the same `ok` without a new email.
pub async fn resend_verification_handler(
    State(state): State<AppState>,
    tenant: crate::state::Tenant,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let user_id: uuid::Uuid = tenant
        .user_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let tenant_id: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let row = sqlx::query("SELECT email, email_verified_at FROM users WHERE id = $1 AND tenant_id = $2")
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
    let verified: Option<chrono::DateTime<chrono::Utc>> = row
        .try_get("email_verified_at")
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    if verified.is_some() {
        return Ok(Json(serde_json::json!({ "ok": true })));
    }
    let raw = issue_verification_token(&state.pool, user_id, 72)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "token", e))?;
    let link = format!("{}/verify?token={raw}", public_base_url());
    let body = format!(
        "Confirm this address to secure your Warden Trade account:\n{link}\n\nThis link expires in 72 hours. If you did not ask for it, ignore this message."
    );
    crate::mail::Mailer::from_env()
        .send(&email, "Warden Trade: verify your email", &body)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "mail", e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// `POST /api/auth/reset/request`: start a password reset. Always `ok`,
/// whether or not the address exists — the address book never leaks here.
/// Real users get a 1-hour single-use link; unknown addresses get silence.
#[derive(Debug, Clone, Deserialize)]
pub struct ResetRequest {
    pub email: String,
}

pub async fn request_reset_handler(
    State(state): State<AppState>,
    Json(req): Json<ResetRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let email = normalize_email(&req.email);
    if valid_email(&email) {
        let row: Option<(uuid::Uuid,)> = sqlx::query_as("SELECT id FROM users WHERE email = $1")
            .bind(&email)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
        if let Some((user_id,)) = row {
            match issue_reset_token(&state.pool, user_id, 1).await {
                Ok(raw) => {
                    let link = format!("{}/reset?token={raw}", public_base_url());
                    let body = format!(
                        "A password reset was requested for your Warden Trade account.\n\nChoose a new password here (1 hour, single use):\n{link}\n\nIf that was not you, ignore this message — your password is unchanged."
                    );
                    if let Err(e) = crate::mail::Mailer::from_env()
                        .send(&email, "Warden Trade: reset your password", &body)
                        .await
                    {
                        tracing::warn!("reset email failed for {email}: {e}");
                    }
                }
                Err(e) => tracing::warn!("reset token failed for {email}: {e}"),
            }
        }
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Replace any outstanding reset token, return the fresh raw value.
async fn issue_reset_token(
    pool: &sqlx::PgPool,
    user_id: uuid::Uuid,
    ttl_hours: i64,
) -> Result<String, sqlx::Error> {
    let raw = new_raw_token();
    let hash = token_hash(&raw);
    sqlx::query("DELETE FROM password_reset_tokens WHERE user_id = $1")
        .bind(user_id)
        .execute(&*pool)
        .await?;
    sqlx::query(
        "INSERT INTO password_reset_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, now() + $3::interval)",
    )
    .bind(user_id)
    .bind(&hash)
    .bind(format!("{ttl_hours} hours"))
    .execute(&*pool)
    .await?;
    Ok(raw)
}

/// `GET /reset?token=…`: renders the new-password form only when the token
/// is live; unknown and expired tokens get the same failure page.
#[derive(askama::Template)]
#[template(path = "reset.html")]
pub struct ResetTemplate {
    pub token: String,
    pub ok: bool,
}

pub async fn reset_page_handler(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<VerifyQuery>,
) -> axum::response::Html<String> {
    let live = reset_token_user(&state.pool, &query.token).await.ok().flatten().is_some();
    axum::response::Html(
        ResetTemplate { token: query.token, ok: live }.render().unwrap_or_default(),
    )
}

/// Look up the live user behind a reset token without consuming it.
async fn reset_token_user(
    pool: &sqlx::PgPool,
    raw: &str,
) -> Result<Option<uuid::Uuid>, sqlx::Error> {
    let hash = token_hash(raw);
    let row = sqlx::query("SELECT user_id, expires_at FROM password_reset_tokens WHERE token_hash = $1")
        .bind(&hash)
        .fetch_optional(&*pool)
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    use sqlx::Row;
    let user_id: uuid::Uuid = row.try_get("user_id")?;
    let expires_at: chrono::DateTime<chrono::Utc> = row.try_get("expires_at")?;
    if expires_at < chrono::Utc::now() {
        return Ok(None);
    }
    Ok(Some(user_id))
}

/// `POST /api/auth/reset/confirm`: swap a live token for a new password.
/// The password version bumps, so every session minted before the reset
/// dies immediately — including sessions on devices the owner lost.
#[derive(Debug, Clone, Deserialize)]
pub struct ResetConfirm {
    pub token: String,
    pub new_password: String,
}

pub async fn confirm_reset_handler(
    State(state): State<AppState>,
    Json(req): Json<ResetConfirm>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if req.new_password.len() < MIN_PASSWORD_LEN {
        return Err((StatusCode::BAD_REQUEST, format!("new password must be at least {MIN_PASSWORD_LEN} characters")));
    }
    let user_id = reset_token_user(&state.pool, &req.token)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?
        .ok_or((StatusCode::UNAUTHORIZED, "reset link is invalid or expired".to_string()))?;
    let new_hash = crate::auth::hash_password(&req.new_password)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "hash failed".to_string()))?;
    let hash = token_hash(&req.token);
    sqlx::query("DELETE FROM password_reset_tokens WHERE token_hash = $1")
        .bind(&hash)
        .execute(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    sqlx::query("UPDATE users SET password_hash = $1, pwd_ver = pwd_ver + 1 WHERE id = $2")
        .bind(&new_hash)
        .bind(user_id)
        .execute(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Account data shown on the settings page. Never a hash, never a token.
#[derive(askama::Template)]
#[template(path = "account.html")]
pub struct AccountTemplate {
    pub email: String,
    pub role: String,
    pub member_since: String,
    pub email_verified: bool,
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
        "SELECT email, role, created_at, email_verified_at FROM users WHERE id = $1 AND tenant_id = $2",
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
    let verified: Option<chrono::DateTime<chrono::Utc>> = row
        .try_get("email_verified_at")
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    let page = AccountTemplate {
        email,
        role,
        member_since: created.format("%Y-%m-%d").to_string(),
        email_verified: verified.is_some(),
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
            email_verified: false,
        };
        let html = page.render().expect("account renders");
        assert!(html.contains("trader@example.com"), "account must show the email");
        assert!(html.contains("member"), "account must show the role");
        assert!(html.contains("2026-11-09"), "account must show member-since");
        assert!(html.contains("Unverified"), "account must flag unverified addresses");
        assert!(html.contains("/api/auth/verify/resend"), "unverified account must offer a resend");
        assert!(html.contains("/api/auth/change-password"), "account must wire the password form");
        assert!(html.contains("password-eye"), "password fields must offer show/hide toggles");
        assert!(!html.contains("password_hash"), "template must never touch hashes, got {html:?}");
        assert!(!html.contains("localStorage"), "no client-side secret storage");
        let verified_page = AccountTemplate {
            email: "trader@example.com".to_string(),
            role: "member".to_string(),
            member_since: "2026-11-09".to_string(),
            email_verified: true,
        };
        let verified_html = verified_page.render().expect("verified account renders");
        assert!(verified_html.contains("Verified"), "account must flag verified addresses");
        assert!(!verified_html.contains("id=\"verify-resend\""), "verified account must not offer a resend");
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

    #[tokio::test]
    async fn email_verification_roundtrip() {
        let _lock = super::page_tests::signup_test_lock();
        let prev = std::env::var("SIGNUP_ENABLED").ok();
        std::env::set_var("SIGNUP_ENABLED", "1");
        let Some(state) = proof_state() else {
            super::page_tests::restore_signup_env(prev);
            return;
        };
        let email = format!("verify{}@example.com", chrono::Utc::now().timestamp_millis());

        // Signup issues a verification token row (log-mode mailer, no SMTP).
        let (status, _, Json(_)) = signup_handler(
            State(state.clone()),
            Json(SignupRequest { email: email.clone(), password: "correct-horse-10".to_string() }),
        )
        .await
        .expect("signup must succeed");
        assert_eq!(status, StatusCode::CREATED);
        use sqlx::Row;
        let user_id: uuid::Uuid = sqlx::query("SELECT id FROM users WHERE email = $1")
            .bind(&email)
            .fetch_one(&state.pool)
            .await
            .expect("user row")
            .try_get("id")
            .expect("id");
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM email_verification_tokens WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&state.pool)
                .await
                .expect("token row");
        assert_eq!(pending, 1, "signup must issue exactly one verification token");

        // Consume a token we mint ourselves: unknown raw value first.
        let bad = verify_page_handler(
            State(state.clone()),
            axum::extract::Query(VerifyQuery { token: "not-a-real-token".to_string() }),
        )
        .await;
        assert!(bad.0.contains("invalid or expired"), "unknown token must fail, got {:?}", bad.0);
        let still: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT email_verified_at FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_one(&state.pool)
                .await
                .expect("flag");
        assert!(still.is_none(), "unknown token must not verify");

        // Known raw token: single use stamps the address, then burns.
        let raw = super::new_raw_token();
        let hash = super::token_hash(&raw);
        sqlx::query(
            "INSERT INTO email_verification_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, now() + '1 hour'::interval)",
        )
        .bind(user_id)
        .bind(&hash)
        .execute(&state.pool)
        .await
        .expect("seed token");
        let good = verify_page_handler(State(state.clone()), axum::extract::Query(VerifyQuery { token: raw.clone() })).await;
        assert!(good.0.contains("confirmed"), "valid token must verify, got {:?}", good.0);
        let stamped: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT email_verified_at FROM users WHERE id = $1")
                .bind(user_id)
                .fetch_one(&state.pool)
                .await
                .expect("flag");
        assert!(stamped.is_some(), "valid token must stamp email_verified_at");
        let replay = verify_page_handler(State(state.clone()), axum::extract::Query(VerifyQuery { token: raw })).await;
        assert!(replay.0.contains("invalid or expired"), "token must be single-use, got {:?}", replay.0);
        super::page_tests::restore_signup_env(prev);
    }

    #[tokio::test]
    async fn password_reset_roundtrip() {
        let _lock = super::page_tests::signup_test_lock();
        let prev = std::env::var("SIGNUP_ENABLED").ok();
        std::env::set_var("SIGNUP_ENABLED", "1");
        let Some(state) = proof_state() else {
            super::page_tests::restore_signup_env(prev);
            return;
        };
        let email = format!("reset{}@example.com", chrono::Utc::now().timestamp_millis());
        let (status, _, Json(created)) = signup_handler(
            State(state.clone()),
            Json(SignupRequest { email: email.clone(), password: "correct-horse-10".to_string() }),
        )
        .await
        .expect("signup must succeed");
        assert_eq!(status, StatusCode::CREATED);
        let old_token = created.token;

        // Unknown address: same ok, no token row, nothing leaks.
        let before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM password_reset_tokens").fetch_one(&state.pool).await.expect("count");
        let Json(unknown) = request_reset_handler(
            State(state.clone()),
            Json(ResetRequest { email: "nobody-knows-this@example.com".to_string() }),
        )
        .await
        .expect("unknown address must still return ok");
        assert_eq!(unknown, serde_json::json!({ "ok": true }));
        let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM password_reset_tokens").fetch_one(&state.pool).await.expect("count");
        assert_eq!(before, after, "unknown address must not mint a token");

        // Known address: one token row appears.
        let Json(known) = request_reset_handler(State(state.clone()), Json(ResetRequest { email: email.clone() })).await.expect("request must succeed");
        assert_eq!(known, serde_json::json!({ "ok": true }));
        use sqlx::Row;
        let user_id: uuid::Uuid = sqlx::query("SELECT id FROM users WHERE email = $1")
            .bind(&email)
            .fetch_one(&state.pool)
            .await
            .expect("user row")
            .try_get("id")
            .expect("id");
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM password_reset_tokens WHERE user_id = $1")
                .bind(user_id)
                .fetch_one(&state.pool)
                .await
                .expect("token row");
        assert_eq!(pending, 1, "reset request must issue exactly one token");

        // Bad token and short password both rejected, session untouched.
        let bad = confirm_reset_handler(
            State(state.clone()),
            Json(ResetConfirm { token: "bogus".to_string(), new_password: "a-brand-new-password-10".to_string() }),
        )
        .await;
        assert!(matches!(bad, Err((StatusCode::UNAUTHORIZED, _))), "bogus token must 401, got {bad:?}");
        let raw = super::new_raw_token();
        let hash = super::token_hash(&raw);
        sqlx::query(
            "INSERT INTO password_reset_tokens (user_id, token_hash, expires_at) VALUES ($1, $2, now() + '1 hour'::interval)",
        )
        .bind(user_id)
        .bind(&hash)
        .execute(&state.pool)
        .await
        .expect("seed reset token");
        let short = confirm_reset_handler(
            State(state.clone()),
            Json(ResetConfirm { token: raw.clone(), new_password: "short".to_string() }),
        )
        .await;
        assert!(matches!(short, Err((StatusCode::BAD_REQUEST, _))), "short password must 400, got {short:?}");

        // Valid confirm: password swaps, old password dead, old JWT dead.
        let Json(ok) = confirm_reset_handler(
            State(state.clone()),
            Json(ResetConfirm { token: raw.clone(), new_password: "a-brand-new-password-10".to_string() }),
        )
        .await
        .expect("confirm must succeed");
        assert_eq!(ok, serde_json::json!({ "ok": true }));
        let stale_login = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: "correct-horse-10".to_string() }),
        )
        .await;
        assert!(matches!(stale_login, Err((StatusCode::UNAUTHORIZED, _))), "old password must die, got {stale_login:?}");
        let _ = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: "a-brand-new-password-10".to_string() }),
        )
        .await
        .expect("new password must work");
        use axum::body::Body;
        use axum::extract::FromRequestParts;
        let req = axum::http::Request::builder()
            .uri("/")
            .header("Cookie", format!("warden_token={old_token}"))
            .body(Body::empty())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let rejected = crate::state::Tenant::from_request_parts(&mut parts, &state).await;
        assert!(rejected.is_err(), "pre-reset JWT must die with the password version bump");
        let again = confirm_reset_handler(
            State(state.clone()),
            Json(ResetConfirm { token: raw, new_password: "another-new-password-10".to_string() }),
        )
        .await;
        assert!(matches!(again, Err((StatusCode::UNAUTHORIZED, _))), "reset token must be single-use, got {again:?}");
        super::page_tests::restore_signup_env(prev);
    }
}
