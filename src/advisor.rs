//! Optional LLM advisor: read-only counsel, never trades.
//!
//! The advisor asks an OpenAI-compatible chat API (OpenRouter, OpenAI, …)
//! for a stance on one symbol, records the opinion in `advisor_opinions`,
//! and stops there. Nothing in the trading path ([`crate::scheduler`],
//! [`crate::orders`]) reads these rows: the bot keeps deciding alone
//! while the advisor's track record is measured on real market data.
//!
//! Disabled without **both** `LLM_API_KEY` and `LLM_MODEL` — that is the
//! normal state until the owner pastes a key. Disabled means zero HTTP:
//! [`tick_advisor`] returns `None` before touching the network or the DB.
//!
//! Safety rules (same as the rest of the codebase):
//! - the key travels only in the `Authorization` header, never in a URL,
//!   a prompt, a log line or an error string;
//! - 20 s timeout so a slow model never stalls the 60 s scheduler tick;
//! - at most one symbol per tick (round-robin over the overdue ones),
//!   so API spend stays flat and predictable;
//! - temperature 0 + strict JSON schema: anything unexpected becomes
//!   `abstained`, never a trade signal.

use std::time::Duration;

use axum::Json;

/// Default chat-completions endpoint (OpenCode Go: spends the owner's Go
/// quota, not API credits). Override with `LLM_BASE_URL` for another
/// provider or proxy.
pub const DEFAULT_BASE_URL: &str = "https://opencode.ai/zen/go/v1";

/// Stable session id required by the Go endpoint for routing and prompt
/// caching. One per deployment is enough: all advisor calls belong to the
/// same logical conversation.
pub const ADVISOR_SESSION_ID: &str = "warden-trade-advisor-01";

/// Hard cap on one advisor HTTP round-trip. Measured 2-6 s warm, ~14 s
/// cold: 20 s cut the very first call of a fresh process. 50 s keeps one
/// slow counsel from stalling the 60 s scheduler for more than a tick.
pub const ADVISOR_TIMEOUT_SECS: u64 = 50;

/// Headlines quoted in the prompt (newest first, truncated per title).
const PROMPT_MAX_TITLES: usize = 8;
/// Per-title cap so a long headline cannot blow the context window.
const PROMPT_TITLE_CHARS: usize = 120;
/// Rationale stored in DB, truncated server-side before insert.
const RATIONALE_CHARS: usize = 500;

/// One recorded opinion, as served by `GET /api/advisor`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AdvisorOpinion {
    pub symbol: String,
    pub stance: String,
    pub confidence: f64,
    pub rationale: String,
    pub model: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Pure gate: enabled only with a non-empty key **and** model.
/// Tested directly; [`advisor_enabled`] is the env wrapper.
pub fn advisor_enabled_from(key: Option<&str>, model: Option<&str>) -> bool {
    matches!((key, model), (Some(k), Some(m)) if !k.trim().is_empty() && !m.trim().is_empty())
}

/// Env wrapper over [`advisor_enabled_from`].
pub fn advisor_enabled() -> bool {
    advisor_enabled_from(
        std::env::var("LLM_API_KEY").ok().as_deref(),
        std::env::var("LLM_MODEL").ok().as_deref(),
    )
}

/// Minimum seconds between two opinions on the same symbol.
/// Env `ADVISOR_INTERVAL_SECS`, floor 60 s, default 900 s (15 min).
pub fn advisor_interval_secs() -> u64 {
    std::env::var("ADVISOR_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(|v: u64| v.max(60))
        .unwrap_or(900)
}

/// Build the user prompt from live market context. The key is never an
/// input here — it only ever leaves in the HTTP `Authorization` header.
pub fn build_prompt(
    symbol: &str,
    price: f64,
    sentiment: f32,
    funding: Option<f64>,
    fear_greed: Option<u8>,
    titles: &[String],
) -> String {
    let funding_txt = funding
        .map(|f| format!("{:+.4}% per 8h", f * 100.0))
        .unwrap_or_else(|| "unknown".to_string());
    let fg_txt = fear_greed
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let mut headlines = String::new();
    for t in titles.iter().take(PROMPT_MAX_TITLES) {
        let mut short: String = t.chars().take(PROMPT_TITLE_CHARS).collect();
        if short.len() < t.len() {
            short.push('…');
        }
        headlines.push_str("- ");
        headlines.push_str(&short);
        headlines.push('\n');
    }
    if headlines.is_empty() {
        headlines.push_str("(no headlines)\n");
    }
    format!(
        "You are a cautious crypto market analyst. This is PAPER TRADING analysis only: \
no money is at stake, you cannot place orders, your text is recorded for later scoring. \
\n\nAsset: {symbol}\nLast price: {price:.2}\nNews sentiment score: {sentiment:+.2} (-1 bearish … +1 bullish)\n\
Perpetual funding rate: {funding_txt}\nFear & Greed index: {fg_txt} (0 extreme fear … 100 extreme greed)\n\
Recent headlines:\n{headlines}\nReply with JSON ONLY, no markdown, no prose: \
{{\"stance\": \"bullish\"|\"bearish\"|\"neutral\", \"confidence\": 0.0-1.0, \"rationale\": \"one sentence\"}}"
    )
}

/// Parse a chat-completions body into `(stance, confidence, rationale)`.
/// Tolerates ` ```json ` fences around the JSON. Anything off-schema
/// (unknown stance, missing field, bad JSON) becomes `None` → recorded
/// as `abstained`, never retried blindly.
pub fn parse_opinion(body: &str) -> Option<(String, f64, String)> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let content = v
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()?;
    parse_stance_json(content)
}

/// Parse the model `content` string itself (fenced or raw JSON).
fn parse_stance_json(content: &str) -> Option<(String, f64, String)> {
    let raw = content.trim();
    let raw = raw
        .strip_prefix("```json")
        .or_else(|| raw.strip_prefix("```"))
        .unwrap_or(raw);
    let raw = raw.strip_suffix("```").unwrap_or(raw).trim();
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let stance = v.get("stance")?.as_str()?.to_lowercase();
    if stance != "bullish" && stance != "bearish" && stance != "neutral" {
        return None;
    }
    let confidence: f64 = v.get("confidence")?.as_f64()?.clamp(0.0, 1.0);
    let rationale: String = v
        .get("rationale")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .chars()
        .take(RATIONALE_CHARS)
        .collect();
    Some((stance, confidence, rationale))
}

/// One HTTP round-trip to the chat API. Returns the parsed opinion or
/// `None` (network error, timeout, off-schema → abstention). The key is
/// a parameter used only as a header value; error strings never contain
/// it because it is never formatted into any message.
async fn request_opinion_with(key: &str, model: &str, base: &str, prompt: &str) -> Option<(String, f64, String)> {
    if key.trim().is_empty() || model.trim().is_empty() {
        return None;
    }
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(ADVISOR_TIMEOUT_SECS))
        .user_agent("warden-trade-advisor/0.3.0")
        .build()
        .ok()?;
    let resp = http
        .post(&url)
        .header("Authorization", format!("Bearer {key}"))
        .header("x-opencode-session", ADVISOR_SESSION_ID)
        .header("HTTP-Referer", "https://warden.djeel.org/")
        .header("X-Title", "Warden Trade paper advisor")
        .json(&serde_json::json!({
            "model": model,
            "temperature": 0,
            // Reasoning models spend most of the budget thinking: 300 caps
            // the reply before it starts (finish_reason=length, null
            // content). 2000 leaves room for ~1k reasoning + the JSON.
            "max_tokens": 2000,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "system", "content": "You reply with JSON only. No markdown, no prose." },
                { "role": "user", "content": prompt },
            ],
        }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    parse_opinion(&body)
}

/// Env wrapper: key/model/base from `LLM_*`, empty when disabled.
async fn request_opinion(prompt: &str) -> Option<(String, f64, String, String)> {
    let key = std::env::var("LLM_API_KEY").ok().filter(|k| !k.trim().is_empty())?;
    let model = std::env::var("LLM_MODEL").ok().filter(|m| !m.trim().is_empty())?;
    let base = std::env::var("LLM_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    request_opinion_with(&key, &model, &base, prompt)
        .await
        .map(|(s, c, r)| (s, c, r, model))
}

/// Latest opinions for this tenant, newest first (for `/api/advisor` and
/// the dashboard card). Empty when the table is missing or unreachable.
pub async fn latest_opinions(
    pool: &sqlx::PgPool,
    tenant_id: uuid::Uuid,
    limit: i64,
) -> Vec<AdvisorOpinion> {
    sqlx::query_as::<_, (String, String, f64, String, String, chrono::DateTime<chrono::Utc>)>(
        "SELECT symbol, stance, confidence, rationale, model, created_at \
         FROM advisor_opinions WHERE tenant_id = $1 \
         ORDER BY created_at DESC LIMIT $2",
    )
    .bind(tenant_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(symbol, stance, confidence, rationale, model, created_at)| AdvisorOpinion {
        symbol,
        stance,
        confidence,
        rationale,
        model,
        created_at,
    })
    .collect()
}

/// Advisor pass, called once per scheduler tick. Returns a one-line
/// summary for the tick log, or `None` when disabled / nothing is due /
/// the opinion abstained.
///
/// Concrete order: no key → `None` before any I/O. One symbol max per
/// tick: the first symbol whose latest opinion is older than
/// [`advisor_interval_secs`] (a symbol with no opinion yet is overdue).
/// The opinion is recorded; it is never consulted for order placement.
pub async fn tick_advisor(state: &crate::state::AppState) -> Option<String> {
    if !advisor_enabled() {
        return None;
    }
    let tenant_id = crate::scheduler::solo_tenant(&state.pool).await?;
    let interval = chrono::Duration::seconds(advisor_interval_secs() as i64);
    let now = chrono::Utc::now();
    let mut due: Option<String> = None;
    for symbol in crate::scheduler::scheduler_symbols() {
        let last: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
            "SELECT max(created_at) FROM advisor_opinions WHERE tenant_id = $1 AND symbol = $2",
        )
        .bind(tenant_id)
        .bind(&symbol)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(None);
        let overdue = last.map(|t| now - t > interval).unwrap_or(true);
        if overdue {
            due = Some(symbol);
            break;
        }
    }
    let symbol = due?;
    let base = symbol.split('-').next().unwrap_or(&symbol);
    let closes = crate::orchestrator::live_closes_for(&symbol).await.unwrap_or_default();
    let price = closes.last().copied().filter(|p| p.is_finite() && *p > 0.0).unwrap_or(0.0);
    let rss = crate::news::fetch_sentiment().await;
    let fg = crate::news::fetch_fear_greed().await;
    let sentiment = crate::news::combine_sentiment(rss, fg);
    let funding = crate::news::fetch_funding(base).await;
    let titles = crate::news::fetch_titles(PROMPT_MAX_TITLES).await;
    let prompt = build_prompt(&symbol, price, sentiment, funding, fg, &titles);
    let model = std::env::var("LLM_MODEL").unwrap_or_default();
    let (stance, confidence, rationale) = match request_opinion(&prompt).await {
        Some((s, c, r, _)) => (s, c, r),
        None => ("abstained".to_string(), 0.0, "no usable model reply".to_string()),
    };
    let inserted = sqlx::query(
        "INSERT INTO advisor_opinions (tenant_id, symbol, stance, confidence, rationale, model) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(tenant_id)
    .bind(&symbol)
    .bind(&stance)
    .bind(confidence)
    .bind(&rationale)
    .bind(&model)
    .execute(&state.pool)
    .await
    .is_ok();
    if !inserted {
        return None;
    }
    Some(format!("advisor {symbol} {stance} {confidence:.2}"))
}

/// `GET /api/advisor`: latest recorded opinions for the caller's tenant.
///
/// Requires login ([`Tenant`]). Read-only; an empty array means disabled
/// or nothing recorded yet — never an error to display.
pub async fn advisor_handler(
    tenant: crate::state::Tenant,
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
) -> Json<Vec<AdvisorOpinion>> {
    let tenant_id: uuid::Uuid = tenant.tenant_id.parse().unwrap_or_default();
    Json(latest_opinions(&state.pool, tenant_id, 20).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_needs_key_and_model() {
        assert!(advisor_enabled_from(Some("k"), Some("m")));
        assert!(!advisor_enabled_from(None, Some("m")));
        assert!(!advisor_enabled_from(Some("k"), None));
        assert!(!advisor_enabled_from(Some("  "), Some("m")));
        assert!(!advisor_enabled_from(Some("k"), Some("")));
    }

    #[test]
    fn prompt_carries_context_not_secrets() {
        let titles = vec!["Bitcoin surge to record high on ETF inflows".to_string()];
        let p = build_prompt("BTC-USD", 75000.0, 0.42, Some(0.0001), Some(72), &titles);
        assert!(p.contains("BTC-USD"), "prompt must name the asset");
        assert!(p.contains("75000"), "prompt must carry the price");
        assert!(p.contains("surge"), "prompt must quote headlines");
        assert!(p.contains("PAPER TRADING"), "prompt must scope the model");
        assert!(!p.contains("sk-"), "prompt must never carry a key");
    }

    #[test]
    fn prompt_truncates_headline_firehose() {
        let titles: Vec<String> = (0..50).map(|i| format!("headline number {i} with filler words")).collect();
        let p = build_prompt("SOL-USD", 90.0, 0.0, None, None, &titles);
        assert!(!p.contains("headline number 49"), "only the first titles are quoted");
        assert!(p.contains("headline number 0"));
    }

    #[test]
    fn parses_clean_completion_body() {
        let body = r#"{"id":"x","choices":[{"message":{"role":"assistant","content":"{\"stance\":\"bullish\",\"confidence\":0.7,\"rationale\":\"ETF inflows persist.\"}"}}]}"#;
        let (stance, conf, rationale) = parse_opinion(body).expect("valid body must parse");
        assert_eq!(stance, "bullish");
        assert!((conf - 0.7).abs() < 1e-9);
        assert!(rationale.contains("ETF"));
    }

    #[test]
    fn parses_fenced_content() {
        let body = r#"{"choices":[{"message":{"content":"```json\n{\"stance\": \"bearish\", \"confidence\": 0.2, \"rationale\": \"Funding overcrowded.\"}\n```"}}]}"#;
        let (stance, conf, _) = parse_opinion(body).expect("fenced content must parse");
        assert_eq!(stance, "bearish");
        assert!((conf - 0.2).abs() < 1e-9);
    }

    #[test]
    fn rejects_off_schema_without_retry_signal() {
        for bad in [
            r#"{"choices":[]}"#,
            r#"{"choices":[{"message":{"content":"{\"stance\":\"moon\",\"confidence\":0.9}"}}]}"#,
            r#"{"choices":[{"message":{"content":"not json at all"}}]}"#,
            r#"{"choices":[{"message":{"content":"{\"confidence\":0.5}"}}]}"#,
            "garbage",
        ] {
            assert!(parse_opinion(bad).is_none(), "must abstain, got {bad:?}");
        }
    }

    #[test]
    fn clamps_confidence_into_range() {
        let body = r#"{"choices":[{"message":{"content":"{\"stance\":\"neutral\",\"confidence\":9.5,\"rationale\":\"x\"}"}}]}"#;
        let (_, conf, _) = parse_opinion(body).expect("must parse");
        assert!((conf - 1.0).abs() < 1e-9, "confidence must clamp to 1, got {conf}");
    }

    #[tokio::test]
    async fn disabled_advisor_touches_nothing() {
        // No LLM_* env in tests → gate closed → None before any I/O,
        // even with an unreachable pool.
        let state = crate::state::AppState {
            pool: sqlx::PgPool::connect_lazy("postgres://127.0.0.1:1/none")
                .expect("test lazy pool"),
            jwt_secret: "test".to_string(),
            order_gate: Default::default(),
        };
        assert!(tick_advisor(&state).await.is_none());
    }

    #[tokio::test]
    async fn unreachable_provider_abstains_fast() {
        // Closed local port: refused in ms, proves the timeout path
        // without touching process env (parallel-safe).
        let t0 = std::time::Instant::now();
        let out = request_opinion_with("k", "m", "http://127.0.0.1:1", "{}").await;
        assert!(out.is_none(), "closed port must abstain");
        assert!(t0.elapsed() < Duration::from_secs(10), "must fail fast, well under the 20 s cap");
    }

    #[tokio::test]
    async fn empty_key_never_dials() {
        let out = request_opinion_with("", "m", "http://127.0.0.1:1", "{}").await;
        assert!(out.is_none());
    }
}
