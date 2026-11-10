//! Strategy lab (provider-agnostic): any OpenAI-compatible LLM proposes
//! parameter sets, Rust backtests them, only measured edge reaches paper.
//!
//! Pipeline (each stage can reject, rejection is the normal outcome):
//! 1. **Propose** — the configured model (`LLM_API_KEY` + `LLM_MODEL` +
//!    optional `LLM_BASE_URL`, same plumbing as the advisor) returns a
//!    JSON array of [`StrategySpec`]. No key/model → no proposals, the
//!    lab still evaluates hand-written specs.
//! 2. **Validate** — [`validate_spec`] accepts only the known strategy
//!    family with sane bounds. Unknown names, unknown fields, and
//!    degenerate windows are rejected before any backtest.
//! 3. **Backtest** — [`evaluate_offline`] runs the *same rule object*
//!    the live loop would vote with ([`crate::strategies::SmaCross`]
//!    with the proposed windows) over daily closes. No parallel
//!    re-implementation.
//! 4. **Gate** — [`promotion_gate`] promotes only specs with enough
//!    trades, net-positive PnL after fees, and bounded drawdown.
//! 5. **Journal** — every evaluation lands in `strategy_journal`
//!    (tenant-scoped), promoted or not. The journal is the audit trail
//!    the next proposal round reads.
//!
//! Nothing here places orders or touches live state. Paper only, like
//! everything else in this codebase.

use std::time::Duration;

use axum::{
    Json,
    extract::{Query, State},
};
use serde::{Deserialize, Serialize};

/// Known strategy families the lab can instantiate. Anything else is
/// rejected at validation — the lab never `eval`s model output.
pub const KNOWN_STRATEGIES: &[&str] = &["sma-cross"];

/// Validation bounds for SMA windows.
pub const MIN_WINDOW: usize = 1;
pub const MAX_WINDOW: usize = 50;
/// Promotion gate: minimum completed trades for a verdict to mean anything.
pub const MIN_TRADES: usize = 5;
/// Promotion gate: maximum drawdown as % of the series mean price.
pub const MAX_DRAWDOWN_PCT: f64 = 15.0;
/// History the lab evaluates on (daily closes, same feed as the backtest tab).
pub const LAB_GRANULARITY: u64 = 86400;
/// Cap on specs evaluated per request (each one is a full backtest).
pub const MAX_SPECS_PER_REQUEST: usize = 8;
/// Cap on journal rows returned per request.
pub const MAX_JOURNAL_ROWS: i64 = 50;

/// A model-proposed (or hand-written) parameter set, strict JSON:
/// unknown fields are rejected, not ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrategySpec {
    /// Strategy family, one of [`KNOWN_STRATEGIES`].
    pub strategy: String,
    /// Fast SMA window in prices.
    pub fast: usize,
    /// Slow SMA window in prices.
    pub slow: usize,
}

/// Backtest verdict for one spec, journaled verbatim.
#[derive(Debug, Clone, Serialize)]
pub struct LabReport {
    pub spec: StrategySpec,
    pub trades: usize,
    pub wins: usize,
    pub win_rate: f64,
    pub total_pnl: f64,
    pub max_drawdown: f64,
    pub promoted: bool,
    pub gate_reason: String,
}

/// Instantiate the live rule object for a validated spec.
///
/// Only [`KNOWN_STRATEGIES`] resolve; everything else is `Err` — the
/// caller never sees a strategy the live loop cannot vote with.
pub fn strategy_for_spec(spec: &StrategySpec) -> Result<crate::strategies::SmaCross, String> {
    validate_spec(spec)?;
    Ok(crate::strategies::SmaCross {
        fast: spec.fast,
        slow: spec.slow,
    })
}

/// Accept only the known family with 1 ≤ fast < slow ≤ 50.
pub fn validate_spec(spec: &StrategySpec) -> Result<(), String> {
    if !KNOWN_STRATEGIES.contains(&spec.strategy.as_str()) {
        return Err(format!(
            "unknown strategy '{}' (known: {})",
            spec.strategy,
            KNOWN_STRATEGIES.join(", ")
        ));
    }
    if spec.fast < MIN_WINDOW || spec.slow > MAX_WINDOW {
        return Err(format!(
            "windows out of bounds (need {MIN_WINDOW} <= fast, slow <= {MAX_WINDOW})"
        ));
    }
    if spec.fast >= spec.slow {
        return Err("degenerate windows (need fast < slow)".to_string());
    }
    Ok(())
}

/// Backtest one spec over `closes` and apply the promotion gate.
/// Pure: no I/O, no DB — journaling is the handler's job.
pub fn evaluate_offline(spec: &StrategySpec, closes: &[f64]) -> LabReport {
    let failed = |reason: &str| LabReport {
        spec: spec.clone(),
        trades: 0,
        wins: 0,
        win_rate: 0.0,
        total_pnl: 0.0,
        max_drawdown: 0.0,
        promoted: false,
        gate_reason: reason.to_string(),
    };
    let rule = match strategy_for_spec(spec) {
        Ok(rule) => rule,
        Err(reason) => return failed(&reason),
    };
    let result = crate::backtest::run_backtest_strategy(&rule, closes);
    let (promoted, gate_reason) = promotion_gate(&result, closes);
    LabReport {
        spec: spec.clone(),
        trades: result.trades,
        wins: result.wins,
        win_rate: result.win_rate_f64,
        total_pnl: result.total_pnl_f64,
        max_drawdown: result.max_drawdown_f64,
        promoted,
        gate_reason: gate_reason.to_string(),
    }
}

/// Conservative promotion gate: enough trades to mean something,
/// net-positive after fees, drawdown bounded relative to price level.
/// Returns `(promoted, reason)` — the reason is journaled either way.
fn promotion_gate(result: &crate::backtest::BacktestResult, closes: &[f64]) -> (bool, &'static str) {
    if result.trades < MIN_TRADES {
        return (false, "too few trades");
    }
    if result.total_pnl_f64 <= 0.0 {
        return (false, "no net edge after fees");
    }
    let mean = closes.iter().sum::<f64>() / closes.len().max(1) as f64;
    if mean > 0.0 && result.max_drawdown_f64 / mean * 100.0 > MAX_DRAWDOWN_PCT {
        return (false, "drawdown exceeds gate");
    }
    (true, "edge after fees, drawdown bounded")
}

/// Persist one evaluation to the tenant's journal. Best effort like the
/// rest of the read path: a journal write must never fail an evaluation.
pub async fn record_journal(
    pool: &sqlx::PgPool,
    tenant_id: uuid::Uuid,
    report: &LabReport,
) {
    let spec = serde_json::to_value(&report.spec).unwrap_or(serde_json::Value::Null);
    let _ = sqlx::query(
        "INSERT INTO strategy_journal
         (tenant_id, spec, trades, wins, total_pnl, max_drawdown, promoted, gate_reason)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(tenant_id)
    .bind(spec)
    .bind(report.trades as i32)
    .bind(report.wins as i32)
    .bind(report.total_pnl)
    .bind(report.max_drawdown)
    .bind(report.promoted)
    .bind(&report.gate_reason)
    .execute(pool)
    .await;
}

/// Ask the configured model for candidate specs (any OpenAI-compatible
/// chat endpoint via `LLM_BASE_URL`). Returns the specs that survive
/// strict parsing — malformed entries are dropped, never repaired.
/// `None` without key/model, on network error, or when nothing parses:
/// the lab works fine on hand-written specs alone.
pub async fn fetch_proposals(symbols: &[&str]) -> Option<Vec<StrategySpec>> {
    let key = std::env::var("LLM_API_KEY").ok().filter(|k| !k.trim().is_empty())?;
    let model = std::env::var("LLM_MODEL").ok().filter(|m| !m.trim().is_empty())?;
    let base = std::env::var("LLM_BASE_URL")
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let prompt = format!(
        "You propose parameters for a crypto paper-trading strategy lab. \
         Reply with ONLY a JSON array (no prose, no fences) of 1-4 objects, \
         each {{\"strategy\": \"sma-cross\", \"fast\": <int>, \"slow\": <int>}} \
         with 1 <= fast < slow <= 50. Symbols under test: {}. \
         Prefer diverse windows (fast 2-10, slow 6-30).",
        symbols.join(", ")
    );
    let url = format!("{}/chat/completions", base.trim_end_matches('/'));
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(50))
        .user_agent("warden-trade-lab/0.3.0")
        .build()
        .ok()?;
    let resp = http
        .post(&url)
        .bearer_auth(&key)
        .json(&serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": 512,
        }))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    parse_proposals(&body)
}

/// Strict parse of the chat response: first `[...]` block, array of
/// [`StrategySpec`] with unknown fields rejected. `None` when nothing
/// usable survives.
pub fn parse_proposals(body: &str) -> Option<Vec<StrategySpec>> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let content = v
        .get("choices")?
        .get(0)?
        .get("message")?
        .get("content")?
        .as_str()?;
    let start = content.find('[')?;
    let end = content.rfind(']')?;
    let raw = content[start..=end].trim();
    let specs: Vec<StrategySpec> = serde_json::from_str(raw).ok()?;
    let valid: Vec<StrategySpec> = specs
        .into_iter()
        .filter(|s| validate_spec(s).is_ok())
        .collect();
    if valid.is_empty() { None } else { Some(valid) }
}

#[derive(Debug, Deserialize)]
pub struct LabQuery {
    pub product: Option<String>,
    /// When `"1"`, also ask the model for fresh specs; otherwise only
    /// the built-in candidate grid is evaluated (no LLM call, no cost).
    pub propose: Option<String>,
}

/// `POST /api/lab/evaluate` (login required): backtest candidate specs
/// over daily closes and journal every verdict.
///
/// Body: JSON array of [`StrategySpec`] (max [`MAX_SPECS_PER_REQUEST`]).
/// With `?propose=1` the model is asked first and its surviving specs
/// are appended. Returns the reports, promoted first.
pub async fn evaluate_handler(
    State(state): State<crate::state::AppState>,
    tenant: crate::state::Tenant,
    Query(query): Query<LabQuery>,
    Json(specs): Json<Vec<StrategySpec>>,
) -> Json<Vec<LabReport>> {
    let mut specs = specs;
    if query.propose.as_deref() == Some("1") {
        if let Some(proposed) = fetch_proposals(&["BTC-USD", "ETH-USD", "SOL-USD"]).await {
            specs.extend(proposed);
        }
    }
    specs.truncate(MAX_SPECS_PER_REQUEST);
    let product = query.product.unwrap_or_else(|| "BTC-USD".to_string());
    let tenant_id: uuid::Uuid = match tenant.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Json(vec![]),
    };
    let closes: Vec<f64> = crate::coinbase::CoinbaseClient::new()
        .fetch_candles(&product, LAB_GRANULARITY)
        .await
        .map(|candles| candles.into_iter().map(|c| c.close).collect())
        .unwrap_or_default();
    let mut reports: Vec<LabReport> = specs
        .iter()
        .map(|spec| evaluate_offline(spec, &closes))
        .collect();
    for report in &reports {
        record_journal(&state.pool, tenant_id, report).await;
    }
    reports.sort_by(|a, b| b.promoted.cmp(&a.promoted));
    Json(reports)
}

#[derive(Debug, Serialize, Clone)]
pub struct JournalRow {
    pub spec: serde_json::Value,
    pub trades: i32,
    pub wins: i32,
    pub total_pnl: f64,
    pub max_drawdown: f64,
    pub promoted: bool,
    pub gate_reason: String,
    pub created_at: String,
}

/// `GET /api/lab/journal` (login required): this tenant's recent
/// evaluations, newest first. Empty (not an error) before migration
/// 0010 or before the first evaluation.
pub async fn journal_handler(
    State(state): State<crate::state::AppState>,
    tenant: crate::state::Tenant,
) -> Json<Vec<JournalRow>> {
    let tenant_id: uuid::Uuid = match tenant.tenant_id.parse() {
        Ok(id) => id,
        Err(_) => return Json(vec![]),
    };
    let rows = sqlx::query_as::<_, (serde_json::Value, i32, i32, f64, f64, bool, String, chrono::DateTime<chrono::Utc>)>(
        "SELECT spec, trades, wins, total_pnl, max_drawdown, promoted, gate_reason, created_at
         FROM strategy_journal WHERE tenant_id = $1
         ORDER BY created_at DESC LIMIT $2",
    )
    .bind(tenant_id)
    .bind(MAX_JOURNAL_ROWS)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    Json(
        rows.into_iter()
            .map(
                |(spec, trades, wins, total_pnl, max_drawdown, promoted, gate_reason, created_at)| {
                    JournalRow {
                        spec,
                        trades,
                        wins,
                        total_pnl,
                        max_drawdown,
                        promoted,
                        gate_reason,
                        created_at: created_at.to_rfc3339(),
                    }
                },
            )
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategies::Strategy;

    fn rising(n: usize) -> Vec<f64> {
        (0..n).map(|i| 100.0 + i as f64).collect()
    }

    #[test]
    fn rejects_unknown_strategy() {
        let spec = StrategySpec {
            strategy: "rsi-magic".to_string(),
            fast: 3,
            slow: 8,
        };
        assert!(validate_spec(&spec).is_err());
        assert!(strategy_for_spec(&spec).is_err());
    }

    #[test]
    fn rejects_degenerate_windows() {
        for (fast, slow) in [(0, 8), (8, 8), (9, 8), (1, 51)] {
            let spec = StrategySpec {
                strategy: "sma-cross".to_string(),
                fast,
                slow,
            };
            assert!(validate_spec(&spec).is_err(), "fast={fast} slow={slow}");
        }
    }

    #[test]
    fn rejects_unknown_json_fields() {
        let raw = r#"{"strategy": "sma-cross", "fast": 3, "slow": 8, "leverage": 100}"#;
        assert!(serde_json::from_str::<StrategySpec>(raw).is_err());
    }

    #[test]
    fn accepts_sane_spec_and_builds_live_rule() {
        let spec = StrategySpec {
            strategy: "sma-cross".to_string(),
            fast: 2,
            slow: 5,
        };
        let rule = strategy_for_spec(&spec).expect("valid spec");
        assert_eq!((rule.fast, rule.slow), (2, 5));
    }

    #[test]
    fn default_windows_match_live_rule_vote() {
        // SmaCross::default() must vote exactly like sma_signal on every
        // series: the lab evaluates the live rule, not a lookalike.
        let series = [rising(30), vec![100.0; 30], (0..30).map(|i| 100.0 - i as f64).collect()];
        let rule = crate::strategies::SmaCross::default();
        for prices in &series {
            let a = rule.signal(prices);
            let b = crate::strategies::sma_signal(prices);
            assert_eq!(a.signal, b.signal);
            assert_eq!(a.confidence, b.confidence);
        }
    }

    #[test]
    fn flat_market_has_no_edge_and_is_not_promoted() {
        let spec = StrategySpec {
            strategy: "sma-cross".to_string(),
            fast: 3,
            slow: 8,
        };
        let report = evaluate_offline(&spec, &vec![100.0; 60]);
        assert!(!report.promoted, "flat market must not promote");
    }

    #[test]
    fn invalid_spec_reports_failure_without_panic() {
        let spec = StrategySpec {
            strategy: "nope".to_string(),
            fast: 3,
            slow: 8,
        };
        let report = evaluate_offline(&spec, &rising(60));
        assert!(!report.promoted);
        assert!(report.gate_reason.contains("unknown strategy"));
    }

    #[test]
    fn parses_strict_proposal_array_and_drops_bad_entries() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "[{\"strategy\": \"sma-cross\", \"fast\": 2, \"slow\": 6}, \
                                 {\"strategy\": \"rsi-magic\", \"fast\": 2, \"slow\": 6}, \
                                 {\"strategy\": \"sma-cross\", \"fast\": 9, \"slow\": 4}]"
                }
            }]
        })
        .to_string();
        let specs = parse_proposals(&body).expect("one valid entry survives");
        assert_eq!(specs.len(), 1);
        assert_eq!((specs[0].fast, specs[0].slow), (2, 6));
    }

    #[test]
    fn unparsable_proposal_body_is_none() {
        assert!(parse_proposals("not json at all").is_none());
        assert!(parse_proposals(r#"{"choices": []}"#).is_none());
    }
}
