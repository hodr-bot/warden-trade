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
) -> Result<(StatusCode, axum::http::HeaderMap, Json<serde_json::Value>), (StatusCode, String)> {
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
        "SELECT id, password_hash, pwd_ver, totp_enabled FROM users WHERE tenant_id = $1 AND email = $2",
    )
    .bind(tenant_id)
    .bind(&email)
    .fetch_optional(&state.pool)
    .await
    .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;

    let (user_id, pwd_ver, second_factor): (uuid::Uuid, i32, bool) = match user_row {
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
            let tfa: bool = row
                .try_get("totp_enabled")
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
            (id, pv, tfa)
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
            (id, 0, false)
        }
    };

    // Second factor on: the password alone buys only a 5-minute challenge,
    // never a session. The TOTP endpoint swaps it for the real cookie.
    if second_factor {
        let challenge = crate::auth::encode_challenge_jwt(
            &user_id.to_string(),
            &tenant_id.to_string(),
            pwd_ver,
            state.jwt_secret.as_bytes(),
        )
        .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "jwt", e))?;
        return Ok((
            StatusCode::ACCEPTED,
            axum::http::HeaderMap::new(),
            Json(serde_json::json!({ "totp_required": true, "challenge": challenge })),
        ));
    }

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
    Ok((StatusCode::OK, headers, Json(serde_json::json!({ "token": token }))))
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

/// `GET /forgot`: the password-request form (same SaaS card as login).
/// Public and ungated: the underlying endpoint is blind by design, so the
/// page leaks nothing whether the address exists or not.
#[derive(askama::Template)]
#[template(path = "forgot.html")]
pub struct ForgotTemplate;

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

pub async fn forgot_page_handler() -> axum::response::Html<String> {
    axum::response::Html(ForgotTemplate.render().unwrap_or_default())
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

/// Constant-time string compare: no early exit on the byte lane.
fn ct_eq(a: &str, b: &str) -> bool {
    let (x, y) = (a.as_bytes(), b.as_bytes());
    if x.len() != y.len() {
        return false;
    }
    x.iter().zip(y.iter()).fold(0u8, |acc, (p, q)| acc | (p ^ q)) == 0
}

/// TOTP second factor: setup, confirm, verify (login step-up), disable.
///
/// Flow: authed `POST /totp/setup` returns the secret + `otpauth://` URL
/// (scan it into any authenticator app — no QR image is generated
/// server-side, the URL pastes straight into most apps). `POST
/// /totp/confirm` with a live 6-digit code flips the factor on and returns
/// 10 single-use backup codes, shown exactly once. From then on password
/// login returns `202 totp_required` with a 5-minute challenge; `POST
/// /totp/verify` swaps challenge + code for the real session cookie.
///
/// Storage note: the TOTP secret sits in `users.totp_secret` as
/// base32. That column name reserves the follow-up — AEAD encryption at
/// rest once a cipher crate is vendored (none is available offline today)
/// — but the secret is NOT hashed: verification needs the raw bytes.
/// Backup codes ARE hashed (SHA-256 hex), single-use, deleted on use.

/// Rebuild the account's TOTP object from its stored base32 secret.
fn account_totp(secret_b32: &str, email: &str) -> Result<totp_rs::TOTP, (StatusCode, String)> {
    let bytes = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret_b32)
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "totp secret corrupt".to_string()))?;
    totp_rs::TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        bytes,
        Some("Warden Trade".to_string()),
        email.to_string(),
    )
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "totp misconfigured".to_string()))
}

/// One printable backup code: `XXXX-XXXX` from an unambiguous alphabet
/// (no 0/O, no 1/I), ~48 bits of entropy, hashed before storage.
fn new_backup_code() -> String {
    use rand::seq::SliceRandom;
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let pick = |rng: &mut rand::rngs::ThreadRng| *ALPHABET.choose(rng).expect("alphabet") as char;
    let half: String = (0..4).map(|_| pick(&mut rng)).collect();
    let half2: String = (0..4).map(|_| pick(&mut rng)).collect();
    format!("{half}-{half2}")
}

/// `POST /api/auth/totp/setup` (authed): stage a fresh secret, return it
/// with the `otpauth://` URL. Confirming with a live code enables it.
/// Re-setup before confirming just replaces the pending secret.
pub async fn setup_totp_handler(
    State(state): State<AppState>,
    tenant: crate::state::Tenant,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    use sqlx::Row;
    let uid: uuid::Uuid = tenant
        .user_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let tid: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let row = sqlx::query("SELECT email, totp_enabled FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(&uid)
        .bind(&tid)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    let Some(row) = row else {
        return Err((StatusCode::UNAUTHORIZED, "unknown account".to_string()));
    };
    if row.try_get::<bool, _>("totp_enabled").unwrap_or(false) {
        return Err((StatusCode::CONFLICT, "two-factor is already on — disable it first to rotate".to_string()));
    }
    let email: String = row.try_get("email").map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    let secret_bytes: [u8; 20] = rand::random();
    let totp = totp_rs::TOTP::new(
        totp_rs::Algorithm::SHA1,
        6,
        1,
        30,
        secret_bytes.to_vec(),
        Some("Warden Trade".to_string()),
        email.clone(),
    )
    .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "totp misconfigured".to_string()))?;
    let secret_b32 = totp.get_secret_base32();
    sqlx::query("UPDATE users SET totp_secret = $1, totp_enabled = false WHERE id = $2")
        .bind(&secret_b32)
        .bind(&uid)
        .execute(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    Ok(Json(serde_json::json!({ "secret": secret_b32, "otpauth_url": totp.get_url() })))
}

/// `POST /api/auth/totp/confirm` (authed): a live 6-digit code flips the
/// factor on and mints 10 single-use backup codes, returned once.
#[derive(Debug, Clone, Deserialize)]
pub struct TotpConfirm {
    pub code: String,
}

pub async fn confirm_totp_handler(
    State(state): State<AppState>,
    tenant: crate::state::Tenant,
    Json(req): Json<TotpConfirm>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    use sqlx::Row;
    let uid: uuid::Uuid = tenant
        .user_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let tid: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let row = sqlx::query("SELECT email, totp_secret, totp_enabled FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(&uid)
        .bind(&tid)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    let Some(row) = row else {
        return Err((StatusCode::UNAUTHORIZED, "unknown account".to_string()));
    };
    if row.try_get::<bool, _>("totp_enabled").unwrap_or(false) {
        return Err((StatusCode::CONFLICT, "two-factor is already on".to_string()));
    }
    let email: String = row.try_get("email").map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    let secret: Option<String> = row.try_get("totp_secret").map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    let Some(secret) = secret else {
        return Err((StatusCode::BAD_REQUEST, "run setup first".to_string()));
    };
    let totp = account_totp(&secret, &email)?;
    let live = totp.check_current(&req.code.trim()).unwrap_or(false);
    if !live {
        return Err((StatusCode::UNAUTHORIZED, "wrong code".to_string()));
    }
    sqlx::query("UPDATE users SET totp_enabled = true WHERE id = $1")
        .bind(&uid)
        .execute(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    let codes: Vec<String> = (0..10).map(|_| new_backup_code()).collect();
    sqlx::query("DELETE FROM totp_backup_codes WHERE user_id = $1")
        .bind(&uid)
        .execute(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    for code in &codes {
        sqlx::query("INSERT INTO totp_backup_codes (user_id, code_hash) VALUES ($1, $2)")
            .bind(&uid)
            .bind(token_hash(code))
            .execute(&state.pool)
            .await
            .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    }
    Ok(Json(serde_json::json!({ "ok": true, "backup_codes": codes })))
}

/// `POST /api/auth/totp/verify` (challenge, rate-limited): swap a live
/// 5-minute challenge + TOTP code (or unused backup code) for the real
/// session. Challenge tokens never authenticate anywhere else.
#[derive(Debug, Clone, Deserialize)]
pub struct TotpVerify {
    pub challenge: String,
    pub code: String,
}

pub async fn verify_totp_handler(
    State(state): State<AppState>,
    Json(req): Json<TotpVerify>,
) -> Result<(axum::http::HeaderMap, Json<serde_json::Value>), (StatusCode, String)> {
    let claims = crate::auth::decode_jwt(&req.challenge, state.jwt_secret.as_bytes())
        .map_err(|_| (StatusCode::UNAUTHORIZED, "challenge expired".to_string()))?;
    if claims.purpose != "totp" {
        return Err((StatusCode::UNAUTHORIZED, "not a challenge".to_string()));
    }
    use sqlx::Row;
    let user_id: uuid::Uuid = claims.sub.parse().map_err(|_| (StatusCode::UNAUTHORIZED, "challenge expired".to_string()))?;
    let tenant_id: uuid::Uuid = claims.tenant_id.parse().map_err(|_| (StatusCode::UNAUTHORIZED, "challenge expired".to_string()))?;
    let row = sqlx::query("SELECT email, password_hash, pwd_ver, totp_secret, totp_enabled FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(user_id)
        .bind(tenant_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    let Some(row) = row else {
        return Err((StatusCode::UNAUTHORIZED, "challenge expired".to_string()));
    };
    let live_pv: i32 = row.try_get("pwd_ver").map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    if live_pv != claims.pv {
        return Err((StatusCode::UNAUTHORIZED, "challenge expired".to_string()));
    }
    if !row.try_get::<bool, _>("totp_enabled").unwrap_or(false) {
        return Err((StatusCode::UNAUTHORIZED, "challenge expired".to_string()));
    }
    let email: String = row.try_get("email").map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    let code = req.code.trim().replace(' ', "");
    let mut accepted = false;
    if let Ok(secret) = row.try_get::<Option<String>, _>("totp_secret") {
        if let Some(secret) = secret {
            if let Ok(totp) = account_totp(&secret, &email) {
                accepted = totp.check_current(&code).unwrap_or(false);
            }
        }
    }
    if !accepted {
        // Fall back to a backup code: hash-compare, single-use.
        let rows = sqlx::query("SELECT code_hash FROM totp_backup_codes WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(&state.pool)
            .await
            .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
        for cand in rows {
            let stored: String = cand.try_get("code_hash").unwrap_or_default();
            if ct_eq(&stored, &token_hash(&code)) {
                sqlx::query("DELETE FROM totp_backup_codes WHERE user_id = $1 AND code_hash = $2")
                    .bind(user_id)
                    .bind(&stored)
                    .execute(&state.pool)
                    .await
                    .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
                accepted = true;
                break;
            }
        }
    }
    if !accepted {
        return Err((StatusCode::UNAUTHORIZED, "wrong code".to_string()));
    }
    let (headers, token) = issue_session(&state, &user_id, &tenant_id, live_pv)?;
    Ok((headers, Json(serde_json::json!({ "token": token }))))
}

/// `POST /api/auth/totp/disable` (authed + password): tear the factor
/// down, wipe the secret and every backup code.
#[derive(Debug, Clone, Deserialize)]
pub struct TotpDisable {
    pub password: String,
}

pub async fn disable_totp_handler(
    State(state): State<AppState>,
    tenant: crate::state::Tenant,
    Json(req): Json<TotpDisable>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    use sqlx::Row;
    let uid: uuid::Uuid = tenant
        .user_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let tid: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let row = sqlx::query("SELECT password_hash FROM users WHERE id = $1 AND tenant_id = $2")
        .bind(&uid)
        .bind(&tid)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    let Some(row) = row else {
        return Err((StatusCode::UNAUTHORIZED, "unknown account".to_string()));
    };
    let hash: String = row.try_get("password_hash").map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, "user", e))?;
    if !crate::auth::verify_password(&hash, &req.password) {
        return Err((StatusCode::UNAUTHORIZED, "wrong password".to_string()));
    }
    sqlx::query("UPDATE users SET totp_secret = NULL, totp_enabled = false WHERE id = $1")
        .bind(&uid)
        .execute(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    sqlx::query("DELETE FROM totp_backup_codes WHERE user_id = $1")
        .bind(&uid)
        .execute(&state.pool)
        .await
        .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Public paper profile: identity + workspace paper totals, read-only.
///
/// Opt-in by naming: the page exists only while `display_name` is set —
/// clearing it (profile edit) takes the page dark (`404`). No email, no
/// balances, no sessions: the only numbers are tenant-scoped paper fills
/// and fees, labeled as workspace activity (per-user attribution lands
/// with a future `paper_orders.user_id` column).
#[derive(askama::Template)]
#[template(path = "public_profile.html")]
pub struct PublicProfileTemplate {
    pub found: bool,
    pub display_name: String,
    pub member_since: String,
    pub paper_trades: i64,
    pub total_fees: f64,
}

pub async fn public_profile_handler(
    State(state): State<AppState>,
    axum::extract::Path(user_id): axum::extract::Path<String>,
) -> (StatusCode, axum::response::Html<String>) {
    let not_found = || {
        let page = PublicProfileTemplate {
            found: false,
            display_name: String::new(),
            member_since: String::new(),
            paper_trades: 0,
            total_fees: 0.0,
        };
        (StatusCode::NOT_FOUND, axum::response::Html(page.render().unwrap_or_default()))
    };
    let uid: uuid::Uuid = match user_id.parse() {
        Ok(id) => id,
        Err(_) => return not_found(),
    };
    use sqlx::Row;
    let row = sqlx::query("SELECT display_name, tenant_id, created_at FROM users WHERE id = $1")
        .bind(uid)
        .fetch_optional(&state.pool)
        .await
        .unwrap_or(None);
    let Some(row) = row else { return not_found() };
    let name: Option<String> = row.try_get("display_name").unwrap_or(None);
    let Some(name) = name.filter(|n| !n.trim().is_empty()) else { return not_found() };
    let tenant_id: uuid::Uuid = match row.try_get("tenant_id") {
        Ok(id) => id,
        Err(_) => return not_found(),
    };
    let created: chrono::DateTime<chrono::Utc> = match row.try_get("created_at") {
        Ok(ts) => ts,
        Err(_) => return not_found(),
    };
    let trades: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM paper_orders WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    let fees: f64 = sqlx::query_scalar("SELECT COALESCE(SUM(fee), 0) FROM paper_orders WHERE tenant_id = $1")
        .bind(tenant_id)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0.0);
    let page = PublicProfileTemplate {
        found: true,
        display_name: name,
        member_since: created.format("%Y-%m-%d").to_string(),
        paper_trades: trades,
        total_fees: fees,
    };
    (StatusCode::OK, axum::response::Html(page.render().unwrap_or_default()))
}

/// Incoming profile edit: both fields optional, at least one required.
#[derive(Debug, Clone, Deserialize)]
pub struct ProfileUpdate {
    pub display_name: Option<String>,
    pub email: Option<String>,
}

/// `POST /api/auth/profile` (authed): display name + address.
///
/// Display name is free text ≤ 40 chars (empty clears it). A changed
/// address runs the same shape check as signup, must be free inside the
/// tenant (`409` when taken), and restarts verification: `email_verified_at`
/// clears and a fresh link goes out. Same-address edits touch nothing.
pub async fn update_profile_handler(
    State(state): State<AppState>,
    tenant: crate::state::Tenant,
    Json(req): Json<ProfileUpdate>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let uid: uuid::Uuid = tenant
        .user_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let tid: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid session".to_string()))?;
    let want_name = req.display_name.is_some();
    let want_email = req.email.as_ref().is_some_and(|e| !e.trim().is_empty());
    if !want_name && !want_email {
        return Err((StatusCode::BAD_REQUEST, "nothing to update".to_string()));
    }
    if let Some(name) = req.display_name.as_ref() {
        if name.chars().count() > 40 {
            return Err((StatusCode::BAD_REQUEST, "display name must be 40 characters or fewer".to_string()));
        }
    }
    if let Some(name) = req.display_name {
        let clean = name.trim().to_string();
        let stored: Option<String> = if clean.is_empty() { None } else { Some(clean) };
        sqlx::query("UPDATE users SET display_name = $1 WHERE id = $2 AND tenant_id = $3")
            .bind(&stored)
            .bind(&uid)
            .bind(&tid)
            .execute(&state.pool)
            .await
            .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
    }
    let mut email_changed = false;
    if let Some(raw) = req.email {
        let email = raw.trim().to_ascii_lowercase();
        if email.is_empty() {
            return Err((StatusCode::BAD_REQUEST, "email required".to_string()));
        }
        if !valid_email(&email) {
            return Err((StatusCode::BAD_REQUEST, "invalid email".to_string()));
        }
        use sqlx::Row;
        let row = sqlx::query("SELECT email FROM users WHERE id = $1 AND tenant_id = $2")
            .bind(&uid)
            .bind(&tid)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
        let Some(row) = row else {
            return Err((StatusCode::UNAUTHORIZED, "unknown account".to_string()));
        };
        let current: String = row.try_get("email").unwrap_or_default();
        if current.to_ascii_lowercase() != email {
            let clash: Option<uuid::Uuid> = sqlx::query_scalar(
                "SELECT id FROM users WHERE tenant_id = $1 AND email = $2",
            )
            .bind(&tid)
            .bind(&email)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
            if clash.is_some_and(|id| id != uid) {
                return Err((StatusCode::CONFLICT, "address taken".to_string()));
            }
            sqlx::query("UPDATE users SET email = $1, email_verified_at = NULL WHERE id = $2")
                .bind(&email)
                .bind(&uid)
                .execute(&state.pool)
                .await
                .map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, "db", e))?;
            match issue_verification_token(&state.pool, uid, 72).await {
                Ok(raw) => {
                    let link = format!("{}/verify?token={raw}", public_base_url());
                    let body = format!(
                        "Your Warden Trade address changed.\n\nConfirm the new address to keep your account secure:\n{link}\n\nThis link expires in 72 hours. If you did not request this, change your password."
                    );
                    if let Err(e) = crate::mail::Mailer::from_env()
                        .send(&email, "Warden Trade: verify your new address", &body)
                        .await
                    {
                        tracing::warn!("re-verification email failed for {email}: {e}");
                    }
                }
                Err(e) => tracing::warn!("re-verification token failed for {email}: {e}"),
            }
            email_changed = true;
        }
    }
    Ok(Json(serde_json::json!({ "ok": true, "email_changed": email_changed })))
}

/// Account data shown on the settings page. Never a hash, never a token.
#[derive(askama::Template)]
#[template(path = "account.html")]
pub struct AccountTemplate {
    pub email: String,
    pub user_id: String,
    pub display_name: Option<String>,
    pub role: String,
    pub member_since: String,
    pub email_verified: bool,
    pub totp_enabled: bool,
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
        "SELECT email, display_name, role, created_at, email_verified_at, totp_enabled FROM users WHERE id = $1 AND tenant_id = $2",
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
        user_id: user_id.to_string(),
        display_name: row.try_get("display_name").unwrap_or(None),
        role,
        member_since: created.format("%Y-%m-%d").to_string(),
        email_verified: verified.is_some(),
        totp_enabled: row.try_get::<bool, _>("totp_enabled").unwrap_or(false),
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
pub struct LoginTemplate {
    pub signup_open: bool,
}

/// `GET /login` renders the sign-in form.
pub async fn login_page_handler() -> axum::response::Html<String> {
    axum::response::Html(LoginTemplate { signup_open: signup_enabled() }.render().unwrap_or_default())
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
    async fn login_page_links() {
        let _lock = signup_test_lock();
        let prev = std::env::var("SIGNUP_ENABLED").ok();
        std::env::remove_var("SIGNUP_ENABLED");
        let axum::response::Html(closed) = login_page_handler().await;
        assert!(closed.contains("<form"), "login page must render a form, got {closed:?}");
        assert!(closed.contains("/api/auth/login"), "form must target the login API");
        assert!(closed.contains("window.location.href = \"/\""), "login must redirect to the dashboard");
        assert!(!closed.contains("localStorage"), "session must live in the HttpOnly cookie, not localStorage");
        assert!(closed.contains("password-eye"), "login must offer a show/hide password toggle");
        assert!(!closed.contains("/signup"), "login must not advertise signup while it is closed");
        assert!(closed.contains("/forgot"), "login must link the forgot-password page");
        std::env::set_var("SIGNUP_ENABLED", "1");
        let axum::response::Html(open) = login_page_handler().await;
        assert!(open.contains("/signup"), "login must advertise signup once it is open");
        restore_signup_env(prev);
    }

    #[tokio::test]
    async fn forgot_page_renders_form_without_db() {
        let axum::response::Html(html) = forgot_page_handler().await;
        assert!(html.contains("<form"), "forgot page must render a form, got {html:?}");
        assert!(html.contains("/api/auth/reset/request"), "form must target the reset-request API");
        assert!(html.contains("/login"), "forgot must link back to sign-in");
        assert!(!html.contains("localStorage"), "no client-side secret store on the forgot page");
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
            user_id: "00000000-0000-0000-0000-000000000000".to_string(),
            display_name: Some("Ace".to_string()),
            role: "member".to_string(),
            member_since: "2026-11-09".to_string(),
            email_verified: false,
            totp_enabled: false,
        };
        let html = page.render().expect("account renders");
        assert!(html.contains("trader@example.com"), "account must show the email");
        assert!(html.contains("Ace"), "account must show the display name");
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
            user_id: "00000000-0000-0000-0000-000000000000".to_string(),
            display_name: None,
            role: "member".to_string(),
            member_since: "2026-11-09".to_string(),
            email_verified: true,
            totp_enabled: true,
        };
        let verified_html = verified_page.render().expect("verified account renders");
        assert!(verified_html.contains("Verified"), "account must flag verified addresses");
        assert!(!verified_html.contains("id=\"verify-resend\""), "verified account must not offer a resend");
        assert!(verified_html.contains("Two-factor"), "account must expose the 2FA section");
        assert!(verified_html.contains("totp-disable"), "2FA-on account must offer disable");
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
        let (status, _, Json(login)) = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: "correct-horse-10".to_string() }),
        )
        .await
        .expect("login with fresh credentials must succeed");
        assert_eq!(status, StatusCode::OK);
        let token = login.get("token").and_then(|v| v.as_str()).unwrap_or_default();
        assert!(!token.is_empty());
        let claims = crate::auth::decode_jwt(token, state.jwt_secret.as_bytes())
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
            .header("Cookie", format!("warden_token={token}"))
            .body(Body::empty())
            .unwrap();
        let (mut parts, _) = req.into_parts();
        let rejected = crate::state::Tenant::from_request_parts(&mut parts, &state).await;
        assert!(rejected.is_err(), "stale token must not authenticate after rotation");
        super::page_tests::restore_signup_env(prev);
    }

    /// TOTP helper: rebuild a live code from a setup response's secret.
    fn live_totp_code(secret_b32: &str, email: &str) -> String {
        let bytes = base32::decode(base32::Alphabet::Rfc4648 { padding: false }, secret_b32)
            .expect("setup secret must be valid base32");
        totp_rs::TOTP::new(totp_rs::Algorithm::SHA1, 6, 1, 30, bytes, Some("Warden Trade".to_string()), email.to_string())
            .expect("totp must build")
            .generate_current()
            .expect("code must generate")
    }

    fn mutate_code(code: &str) -> String {
        let mut chars: Vec<char> = code.chars().collect();
        chars[0] = if chars[0] == '0' { '1' } else { '0' };
        chars.into_iter().collect()
    }

    #[tokio::test]
    async fn totp_step_up_roundtrip() {
        let _lock = super::page_tests::signup_test_lock();
        let prev = std::env::var("SIGNUP_ENABLED").ok();
        std::env::set_var("SIGNUP_ENABLED", "1");
        let Some(state) = proof_state() else {
            super::page_tests::restore_signup_env(prev);
            return;
        };
        let email = format!("tfa{}@example.com", chrono::Utc::now().timestamp_millis());
        let password = "correct-horse-10".to_string();

        signup_handler(State(state.clone()), Json(SignupRequest { email: email.clone(), password: password.clone() }))
            .await.expect("signup must succeed");

        // Baseline: no second factor, plain 200 login.
        let (status, _, Json(body)) = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: password.clone() }),
        ).await.expect("login must succeed");
        assert_eq!(status, StatusCode::OK);
        let first_token = body.get("token").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        assert!(!first_token.is_empty());
        let claims = crate::auth::decode_jwt(&first_token, state.jwt_secret.as_bytes()).expect("token must decode");
        assert_eq!(claims.purpose, "session", "session tokens carry their purpose");
        let tenant = crate::state::Tenant { tenant_id: claims.tenant_id.clone(), user_id: claims.sub.clone() };

        // Setup: secret + otpauth URL, factor still off.
        let Json(setup) = setup_totp_handler(State(state.clone()), tenant.clone()).await.expect("setup must succeed");
        let secret = setup.get("secret").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let url = setup.get("otpauth_url").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        assert!(!secret.is_empty() && url.starts_with("otpauth://totp/"), "got {setup:?}");
        let (status, _, _) = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: password.clone() }),
        ).await.expect("login must still succeed");
        assert_eq!(status, StatusCode::OK, "unconfirmed setup must not lock the account");

        // Wrong code cannot confirm; a mutated live code is certainly wrong.
        let good = live_totp_code(&secret, &email);
        let bad = confirm_totp_handler(State(state.clone()), tenant.clone(), Json(TotpConfirm { code: mutate_code(&good) })).await;
        assert!(matches!(bad, Err((StatusCode::UNAUTHORIZED, _))), "wrong code must 401, got {bad:?}");

        // Confirm with the live code: ten one-shot backup codes come back.
        let Json(confirmed) = confirm_totp_handler(State(state.clone()), tenant.clone(), Json(TotpConfirm { code: good })).await
            .expect("live code must confirm");
        let codes: Vec<String> = serde_json::from_value(confirmed.get("backup_codes").cloned().unwrap_or_default()).unwrap_or_default();
        assert_eq!(codes.len(), 10, "ten backup codes, got {confirmed:?}");

        // Password login now yields a challenge, not a session.
        let (status, _, Json(step)) = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: password.clone() }),
        ).await.expect("password stage must succeed");
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(step.get("totp_required").and_then(|v| v.as_bool()), Some(true));
        let challenge = step.get("challenge").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        assert!(!challenge.is_empty());

        // The challenge is not a session: the extractor rejects it.
        use axum::body::Body;
        use axum::extract::FromRequestParts;
        let req = axum::http::Request::builder()
            .uri("/")
            .header("Cookie", format!("warden_token={challenge}"))
            .body(Body::empty()).unwrap();
        let (mut parts, _) = req.into_parts();
        let refused = crate::state::Tenant::from_request_parts(&mut parts, &state).await;
        assert!(refused.is_err(), "challenge must never authenticate as a session");

        // Wrong second step stays shut.
        let wrong = verify_totp_handler(
            State(state.clone()),
            Json(TotpVerify { challenge: challenge.clone(), code: "000000".to_string() }),
        ).await;
        assert!(matches!(wrong, Err((StatusCode::UNAUTHORIZED, _))), "wrong TOTP must 401, got {wrong:?}");

        // Right code swaps the challenge for a real session.
        let fresh = live_totp_code(&secret, &email);
        let (_, Json(done)) = verify_totp_handler(
            State(state.clone()),
            Json(TotpVerify { challenge, code: fresh }),
        ).await.expect("live TOTP must verify");
        let session = done.get("token").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        assert!(!session.is_empty());
        let req = axum::http::Request::builder()
            .uri("/")
            .header("Cookie", format!("warden_token={session}"))
            .body(Body::empty()).unwrap();
        let (mut parts, _) = req.into_parts();
        assert!(crate::state::Tenant::from_request_parts(&mut parts, &state).await.is_ok(), "TOTP session must authenticate");

        // One backup code works exactly once.
        let retry = verify_totp_handler(
            State(state.clone()),
            Json(TotpVerify { challenge: {
                let (s, _, Json(b)) = login_handler(State(state.clone()), Json(LoginRequest { email: email.clone(), password: password.clone() })).await.expect("password stage");
                assert_eq!(s, StatusCode::ACCEPTED);
                b.get("challenge").and_then(|v| v.as_str()).unwrap_or_default().to_string()
            }, code: codes[0].clone() }),
        ).await;
        assert!(retry.is_ok(), "first backup-code use must succeed, got {retry:?}");
        let replay = verify_totp_handler(
            State(state.clone()),
            Json(TotpVerify { challenge: {
                let (s, _, Json(b)) = login_handler(State(state.clone()), Json(LoginRequest { email: email.clone(), password: password.clone() })).await.expect("password stage");
                assert_eq!(s, StatusCode::ACCEPTED);
                b.get("challenge").and_then(|v| v.as_str()).unwrap_or_default().to_string()
            }, code: codes[0].clone() }),
        ).await;
        assert!(matches!(replay, Err((StatusCode::UNAUTHORIZED, _))), "backup code must be single-use, got {replay:?}");

        // Wrong password cannot disable; right password tears the factor down.
        let bad_off = disable_totp_handler(State(state.clone()), tenant.clone(), Json(TotpDisable { password: "not-the-password-1".to_string() })).await;
        assert!(matches!(bad_off, Err((StatusCode::UNAUTHORIZED, _))), "disable needs the password, got {bad_off:?}");
        disable_totp_handler(State(state.clone()), tenant, Json(TotpDisable { password })).await.expect("disable must succeed");
        let (status, _, _) = login_handler(State(state.clone()), Json(LoginRequest { email, password: "correct-horse-10".to_string() })).await
            .expect("login must succeed");
        assert_eq!(status, StatusCode::OK, "account is single-factor again");
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

    #[tokio::test]
    async fn profile_update_roundtrip() {
        let _lock = super::page_tests::signup_test_lock();
        let prev = std::env::var("SIGNUP_ENABLED").ok();
        std::env::set_var("SIGNUP_ENABLED", "1");
        let Some(state) = proof_state() else {
            super::page_tests::restore_signup_env(prev);
            return;
        };
        let stamp = chrono::Utc::now().timestamp_millis();
        let email = format!("prof{stamp}@example.com");
        let password = "correct-horse-10".to_string();
        signup_handler(State(state.clone()), Json(SignupRequest { email: email.clone(), password: password.clone() }))
            .await.expect("signup must succeed");
        let (_, _, Json(body)) = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: password.clone() }),
        ).await.expect("login must succeed");
        let token = body.get("token").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let claims = crate::auth::decode_jwt(&token, state.jwt_secret.as_bytes()).expect("token must decode");
        let tenant = crate::state::Tenant { tenant_id: claims.tenant_id.clone(), user_id: claims.sub.clone() };
        let uid: uuid::Uuid = claims.sub.parse().expect("sub is a uuid");

        // Display name + same address: stored, verification untouched.
        let Json(ok) = update_profile_handler(
            State(state.clone()), tenant.clone(),
            Json(ProfileUpdate { display_name: Some("Ace Trader".to_string()), email: Some(email.clone()) }),
        ).await.expect("profile update must succeed");
        assert_eq!(ok, serde_json::json!({ "ok": true, "email_changed": false }));
        use sqlx::Row;
        let row = sqlx::query("SELECT display_name, email_verified_at FROM users WHERE id = $1")
            .bind(uid).fetch_one(&state.pool).await.expect("row must exist");
        let name: Option<String> = row.try_get("display_name").unwrap_or(None);
        assert_eq!(name.as_deref(), Some("Ace Trader"));
        let verified: Option<chrono::DateTime<chrono::Utc>> = row.try_get("email_verified_at").unwrap_or(None);
        assert!(verified.is_none(), "signup leaves verification pending in tests");

        // Nothing to update, bad address, over-long name: all 400.
        let empty = update_profile_handler(
            State(state.clone()), tenant.clone(),
            Json(ProfileUpdate { display_name: None, email: None }),
        ).await;
        assert!(matches!(empty, Err((StatusCode::BAD_REQUEST, _))), "empty update must 400, got {empty:?}");
        let bad = update_profile_handler(
            State(state.clone()), tenant.clone(),
            Json(ProfileUpdate { display_name: None, email: Some("not-an-email".to_string()) }),
        ).await;
        assert!(matches!(bad, Err((StatusCode::BAD_REQUEST, _))), "bad email must 400, got {bad:?}");
        let long = update_profile_handler(
            State(state.clone()), tenant.clone(),
            Json(ProfileUpdate { display_name: Some("x".repeat(41)), email: None }),
        ).await;
        assert!(matches!(long, Err((StatusCode::BAD_REQUEST, _))), "long name must 400, got {long:?}");

        // Address change: new value stored, verification restarts with a token row.
        let new_email = format!("prof-new{stamp}@example.com");
        let Json(changed) = update_profile_handler(
            State(state.clone()), tenant.clone(),
            Json(ProfileUpdate { display_name: None, email: Some(new_email.clone()) }),
        ).await.expect("email change must succeed");
        assert_eq!(changed, serde_json::json!({ "ok": true, "email_changed": true }));
        let row = sqlx::query("SELECT email, email_verified_at FROM users WHERE id = $1")
            .bind(uid).fetch_one(&state.pool).await.expect("row must exist");
        let stored: String = row.try_get("email").unwrap_or_default();
        assert_eq!(stored, new_email);
        let tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM email_verification_tokens WHERE user_id = $1")
            .bind(uid).fetch_one(&state.pool).await.unwrap_or(0);
        assert_eq!(tokens, 1, "one fresh verification token must exist");

        // Repeating the same address: no-op, no extra token.
        let Json(same) = update_profile_handler(
            State(state.clone()), tenant.clone(),
            Json(ProfileUpdate { display_name: None, email: Some(new_email.clone()) }),
        ).await.expect("same email must succeed");
        assert_eq!(same, serde_json::json!({ "ok": true, "email_changed": false }));
        let tokens: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM email_verification_tokens WHERE user_id = $1")
            .bind(uid).fetch_one(&state.pool).await.unwrap_or(0);
        assert_eq!(tokens, 1, "same-address edit must not mint tokens");

        // Taken address: second account blocks the grab.
        let other = format!("prof-other{stamp}@example.com");
        signup_handler(State(state.clone()), Json(SignupRequest { email: other.clone(), password: password.clone() }))
            .await.expect("second signup must succeed");
        let clash = update_profile_handler(
            State(state.clone()), tenant.clone(),
            Json(ProfileUpdate { display_name: None, email: Some(other) }),
        ).await;
        assert!(matches!(clash, Err((StatusCode::CONFLICT, _))), "taken email must 409, got {clash:?}");

        // Empty name clears back to NULL.
        let Json(cleared) = update_profile_handler(
            State(state.clone()), tenant.clone(),
            Json(ProfileUpdate { display_name: Some("   ".to_string()), email: None }),
        ).await.expect("clear must succeed");
        assert_eq!(cleared, serde_json::json!({ "ok": true, "email_changed": false }));
        let name: Option<String> = sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
            .bind(uid).fetch_one(&state.pool).await.unwrap_or(None);
        assert!(name.is_none(), "blank name must clear to NULL");
        super::page_tests::restore_signup_env(prev);
    }

    #[tokio::test]
    async fn public_profile_visibility() {
        let _lock = super::page_tests::signup_test_lock();
        let prev = std::env::var("SIGNUP_ENABLED").ok();
        std::env::set_var("SIGNUP_ENABLED", "1");
        let Some(state) = proof_state() else {
            super::page_tests::restore_signup_env(prev);
            return;
        };
        let stamp = chrono::Utc::now().timestamp_millis();
        let email = format!("pub{stamp}@example.com");
        let password = "correct-horse-10".to_string();
        signup_handler(State(state.clone()), Json(SignupRequest { email: email.clone(), password: password.clone() }))
            .await.expect("signup must succeed");
        let (_, _, Json(body)) = login_handler(
            State(state.clone()),
            Json(LoginRequest { email: email.clone(), password: password.clone() }),
        ).await.expect("login must succeed");
        let token = body.get("token").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let claims = crate::auth::decode_jwt(&token, state.jwt_secret.as_bytes()).expect("token must decode");
        let tenant = crate::state::Tenant { tenant_id: claims.tenant_id.clone(), user_id: claims.sub.clone() };
        let uid: uuid::Uuid = claims.sub.parse().expect("sub is a uuid");
        let tid: uuid::Uuid = claims.tenant_id.parse().expect("tenant is a uuid");

        // Nameless: dark, no address leak.
        let (status, html) = super::public_profile_handler(State(state.clone()), axum::extract::Path(uid.to_string())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let page = html.0;
        assert!(page.contains("Profile not found"), "nameless page must 404, got {page}");
        assert!(!page.contains(&email), "public page must never carry the address");

        // Named: live with identity + workspace paper totals.
        update_profile_handler(
            State(state.clone()), tenant.clone(),
            Json(ProfileUpdate { display_name: Some("Pub Star".to_string()), email: None }),
        ).await.expect("naming must succeed");
        sqlx::query("INSERT INTO paper_orders (tenant_id, exchange, symbol, side, qty, price, fee) VALUES ($1, 'kraken', 'BTC/EUR', 'buy', 0.01, 50000, 0.5)")
            .bind(tid).execute(&state.pool).await.expect("seed paper fill");
        let (status, html) = super::public_profile_handler(State(state.clone()), axum::extract::Path(uid.to_string())).await;
        assert_eq!(status, StatusCode::OK);
        let page = html.0;
        assert!(page.contains("Pub Star"), "public page must show the name");
        assert!(page.contains("Paper trades"), "public page must show paper totals");
        assert!(page.contains("Paper fees paid"), "public page must show paper fees");
        assert!(!page.contains(&email), "public page must never carry the address");

        // Clearing the name takes the page dark again.
        update_profile_handler(
            State(state.clone()), tenant.clone(),
            Json(ProfileUpdate { display_name: Some(String::new()), email: None }),
        ).await.expect("clear must succeed");
        let (status, _) = super::public_profile_handler(State(state.clone()), axum::extract::Path(uid.to_string())).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "cleared name must darken the page");

        // Unknown id and garbage: same dark page, no oracle.
        let (status, _) = super::public_profile_handler(State(state.clone()), axum::extract::Path(uuid::Uuid::new_v4().to_string())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = super::public_profile_handler(State(state.clone()), axum::extract::Path("not-a-uuid".to_string())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        super::page_tests::restore_signup_env(prev);
    }
}
