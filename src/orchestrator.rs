//! Orchestrator cycle: deterministic fan-out over the 8-agent roster.
//!
//! [`run_cycle_with_prices`] feeds live closes through the three real
//! strategies ([`crate::strategies`]) and maps each agent role to a
//! concrete decision: market observes, technical votes trend, strategy
//! aggregates, quant sizes, risk vetoes, execution signals, portfolio
//! reports, performance reviews. [`run_cycle`] is the priceless
//! degradation (everything holds). [`run_cycle_handler`]
//! (`POST /api/agents/run`) fetches live Coinbase candles, runs the
//! cycle for the authenticated tenant and persists every decision to
//! `agent_decisions`, so history feeds `GET /api/reconcile`.

use axum::{extract::State, http::StatusCode, Json};

use crate::agents::{AgentDecision, AgentId, AgentKind};
use crate::state::{AppState, Tenant};
use crate::strategies::{aggregate, mean_reversion_signal, momentum_signal, sma_signal, Signal};

/// Stable stub agent id for each kind (see [`AgentKind::id`]).
fn agent_id_for(kind: &AgentKind) -> AgentId {
    AgentId::new(kind.id())
}

fn decide(agent: &str, action: &str, symbol: &str, confidence: f64, reason: String) -> AgentDecision {
    AgentDecision {
        agent: agent.to_string(),
        action: action.to_string(),
        symbol: symbol.to_string(),
        confidence,
        reason,
        created_at: chrono::Utc::now().to_rfc3339(),
    }
}

/// Target paper notional per autonomous order, in quote currency.
pub const TARGET_NOTIONAL: f64 = 100.0;

/// Run one deterministic orchestration cycle for `symbol` on `closes`
/// (oldest first, may be empty).
///
/// Empty history degrades every agent to `hold` (same as the legacy
/// stub cycle). With prices, technical/strategy vote real signals,
/// quant sizes a [`TARGET_NOTIONAL`] order, risk approves or vetoes
/// against conservative defaults, and execution carries the validated
/// intent the scheduler places. No network, no secrets, no I/O.
pub fn run_cycle_with_prices(symbol: &str, closes: &[f64]) -> Vec<AgentDecision> {
    let last = closes.last().copied();
    let mut out = Vec::with_capacity(8);

    // Market: observe.
    match last {
        Some(p) => out.push(decide(
            "market",
            "observe",
            symbol,
            0.5,
            format!("last {symbol} {p:.2} over {} closes", closes.len()),
        )),
        None => out.push(decide("market", "hold", symbol, 0.0, "no price history".to_string())),
    }

    // Technical: SMA trend vote.
    let sma = sma_signal(closes);
    out.push(decide(
        "technical",
        sma.signal.action(),
        symbol,
        sma.confidence,
        format!("{}: {}", sma.strategy, sma.reason),
    ));

    // Strategy: aggregate the three votes.
    let mr = mean_reversion_signal(closes);
    let mom = momentum_signal(closes);
    let votes = [sma.clone(), mr.clone(), mom.clone()];
    let final_signal = aggregate(&votes);
    let voters: Vec<f64> = votes
        .iter()
        .filter(|v| v.signal == final_signal && final_signal != Signal::Hold)
        .map(|v| v.confidence)
        .collect();
    let voter_conf = if voters.is_empty() {
        0.0
    } else {
        voters.iter().sum::<f64>() / voters.len() as f64
    };
    out.push(decide(
        "strategy",
        final_signal.action(),
        symbol,
        if final_signal == Signal::Hold { 0.0 } else { voter_conf },
        format!(
            "{}:{} {}:{} {}:{} => {}",
            sma.strategy,
            sma.signal.action(),
            mr.strategy,
            mr.signal.action(),
            mom.strategy,
            mom.signal.action(),
            final_signal.action()
        ),
    ));

    // Quant: size a TARGET_NOTIONAL order at the last price.
    let qty = last.map(|p| if p > 0.0 { TARGET_NOTIONAL / p } else { 0.0 });
    match (last, qty) {
        (Some(p), Some(q)) if q > 0.0 && p.is_finite() => out.push(decide(
            "quant",
            "size",
            symbol,
            0.5,
            format!("qty {q:.6} for ${TARGET_NOTIONAL:.0} @ {p:.2}"),
        )),
        _ => out.push(decide("quant", "hold", symbol, 0.0, "no price to size".to_string())),
    }

    // Risk: independent veto against conservative defaults.
    let (risk_action, risk_conf, risk_reason) = match (last, qty) {
        (Some(p), Some(q)) if q > 0.0 => {
            let intent = crate::risk::OrderIntent {
                symbol: symbol.to_string(),
                side: final_signal.action().to_string(),
                qty: q,
                price: p,
            };
            match crate::risk::check_order(&crate::risk::RiskLimits::conservative_default(), &intent) {
                Ok(()) => ("approve", 0.9, format!("notional {:.2} within default limits", q * p)),
                Err(e) => ("veto", 1.0, e),
            }
        }
        _ => ("veto", 1.0, "nothing to validate".to_string()),
    };
    out.push(decide("risk", risk_action, symbol, risk_conf, risk_reason));

    // Execution: carry the validated intent (the scheduler places it).
    if final_signal != Signal::Hold && risk_action == "approve" {
        out.push(decide(
            "execution",
            final_signal.action(),
            symbol,
            0.8,
            format!("validated {} intent for scheduler", final_signal.action()),
        ));
    } else {
        out.push(decide(
            "execution",
            "hold",
            symbol,
            0.0,
            "no validated intent".to_string(),
        ));
    }

    // Portfolio + performance: reporting roles (DB-backed reads live
    // in the handlers, not in this pure cycle).
    out.push(decide(
        "portfolio",
        "report",
        symbol,
        0.3,
        "see /api/reconcile for exposure".to_string(),
    ));
    out.push(decide(
        "performance",
        "review",
        symbol,
        0.3,
        "see /api/reconcile for attribution".to_string(),
    ));
    out
}

/// Run one deterministic orchestration cycle for `symbol`.
///
/// Legacy priceless entry point: everything holds. Kept for the
/// degraded path (feed unreachable) and for callers without history.
pub fn run_cycle(symbol: &str) -> Vec<AgentDecision> {
    run_cycle_with_prices(symbol, &[])
}

/// Persist one cycle's decisions for `tenant_id` (history for reconcile).
///
/// One row per decision in `agent_decisions`. Read path for
/// `GET /api/reconcile`; never deletes, never trades.
pub async fn record_decisions(
    pool: &sqlx::PgPool,
    tenant_id: uuid::Uuid,
    decisions: &[AgentDecision],
) -> Result<(), sqlx::Error> {
    for d in decisions {
        sqlx::query(
            "INSERT INTO agent_decisions (tenant_id, agent, action, symbol, confidence, reason)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(tenant_id)
        .bind(&d.agent)
        .bind(&d.action)
        .bind(&d.symbol)
        .bind(d.confidence)
        .bind(&d.reason)
        .execute(pool)
        .await?;
    }
    Ok(())
}

/// `POST /api/agents/run`: execute one cycle on live prices for the
/// authenticated tenant and persist the 8 decisions.
///
/// Requires `Authorization: Bearer *** via [`Tenant`]. DB failures become
/// `503`. When the Coinbase feed is unreachable, degrades to the
/// priceless all-hold cycle instead of failing.
pub async fn run_cycle_handler(
    tenant: Tenant,
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentDecision>>, (StatusCode, String)> {
    let tenant_id: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid tenant".to_string()))?;
    let closes = live_closes().await.unwrap_or_default();
    let decisions = run_cycle_with_prices("BTC-USD", &closes);
    record_decisions(&state.pool, tenant_id, &decisions)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")))?;
    Ok(Json(decisions))
}

/// Fetch recent BTC-USD 5-minute closes from Coinbase, oldest first.
///
/// Pure feed read for the cycle; any failure degrades to an empty
/// series (the cycle then holds). Never panics, never logs secrets.
pub async fn live_closes() -> Result<Vec<f64>, String> {
    let client = crate::coinbase::CoinbaseClient::new();
    let candles = client
        .fetch_candles("BTC-USD", 300)
        .await
        .map_err(|e| format!("feed: {e}"))?;
    Ok(candles.iter().map(|c| c.close).collect())
}

/// Axum handler returning this cycle's decisions for `BTC-USD` as JSON.
///
/// Deterministic stub: no network, no secrets, no I/O.
pub async fn agents_handler() -> Json<Vec<AgentDecision>> {
    Json(run_cycle("BTC-USD"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_returns_8_decisions() {
        let decisions = run_cycle("BTC-USD");
        assert_eq!(decisions.len(), 8, "run_cycle must return 8 decisions");
        let by_agent = |id: &str| decisions.iter().find(|d| d.agent == id).unwrap();
        // Priceless degradation: market/technical/strategy/quant/execution
        // hold, risk vetoes (nothing to validate), portfolio reports,
        // performance reviews.
        for d in &decisions {
            assert_eq!(d.symbol, "BTC-USD");
            let expected = match d.agent.as_str() {
                "risk" => "veto",
                "portfolio" => "report",
                "performance" => "review",
                _ => "hold",
            };
            assert_eq!(d.action, expected, "agent {}", d.agent);
        }
        assert_eq!(by_agent("market").action, "hold");
        let agents: Vec<&str> = decisions.iter().map(|d| d.agent.as_str()).collect();
        assert_eq!(
            agents,
            vec![
                "market",
                "technical",
                "strategy",
                "quant",
                "risk",
                "execution",
                "portfolio",
                "performance"
            ]
        );
    }

    #[test]
    fn cycle_with_uptrend_votes_buy() {
        // Flat base then a final jump: SMA crosses up on the last bar,
        // momentum confirms; mean-reversion dissents (overbought) and
        // loses the majority vote. Risk approves the $100 notional.
        let mut closes = vec![100.0; 19];
        closes.push(112.0);
        let decisions = run_cycle_with_prices("BTC-USD", &closes);
        assert_eq!(decisions.len(), 8);
        let by_agent = |id: &str| decisions.iter().find(|d| d.agent == id).unwrap();
        assert_eq!(by_agent("market").action, "observe");
        assert_eq!(by_agent("technical").action, "buy");
        assert_eq!(by_agent("strategy").action, "buy");
        assert_eq!(by_agent("quant").action, "size");
        assert_eq!(by_agent("risk").action, "approve");
        assert_eq!(by_agent("execution").action, "buy");
        for d in &decisions {
            assert!((0.0..=1.0).contains(&d.confidence), "confidence out of range");
            assert!(!d.created_at.is_empty());
        }
    }

    #[test]
    fn cycle_with_downtrend_votes_sell() {
        let mut closes = vec![112.0; 19];
        closes.push(100.0);
        let decisions = run_cycle_with_prices("BTC-USD", &closes);
        let by_agent = |id: &str| decisions.iter().find(|d| d.agent == id).unwrap();
        assert_eq!(by_agent("technical").action, "sell");
        assert_eq!(by_agent("strategy").action, "sell");
        assert_eq!(by_agent("execution").action, "sell");
    }

    #[test]
    fn cycle_with_flat_market_holds() {
        let closes = vec![100.0; 25];
        let decisions = run_cycle_with_prices("BTC-USD", &closes);
        let by_agent = |id: &str| decisions.iter().find(|d| d.agent == id).unwrap();
        assert_eq!(by_agent("strategy").action, "hold");
        assert_eq!(by_agent("execution").action, "hold");
    }

    #[tokio::test]
    async fn handler_returns_8() {
        let Json(decisions) = agents_handler().await;
        assert_eq!(decisions.len(), 8, "handler must return 8 decisions");
        for d in &decisions {
            assert_eq!(d.symbol, "BTC-USD");
        }
        // Handler output must be JSON-serializable (proves Serialize impl works).
        let v = serde_json::to_value(&decisions).expect("decisions must serialize");
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap().len(), 8);
    }
}
