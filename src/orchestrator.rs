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

/// Market-wide context shaping one cycle:
/// - `sentiment` in `[-1.0, 1.0]` (RSS mood merged with Fear & Greed,
///   see [`crate::news`]). Neutral `0.0` disables the news gate.
/// - `funding`: Binance perpetual funding rate if known (e.g. `0.0001`);
///   crowded longs (high positive) veto buys, crowded shorts veto sells.
///   `None` disables the funding gate.
#[derive(Debug, Clone, Copy)]
pub struct MarketContext {
    pub sentiment: f32,
    pub funding: Option<f64>,
}

impl MarketContext {
    /// Neutral context: price signals decide alone.
    pub fn neutral() -> Self {
        Self { sentiment: 0.0, funding: None }
    }
}

/// News veto threshold: at or beyond ±0.5 the headlines overrule price signals.
pub const SENTIMENT_VETO: f32 = 0.5;

/// Funding veto threshold: at or beyond ±0.05 % per 8 h the perpetual
/// crowd is overcrowded and the cycle steps aside (contrarian gate).
pub const FUNDING_VETO: f64 = 0.0005;

/// Target paper notional per autonomous order, in quote currency.
pub const TARGET_NOTIONAL: f64 = 100.0;

/// Order quantity for `price` under the volatility sizing of `closes`.
///
/// Shared by the quant agent (display) and the scheduler (placement)
/// so both agree on size. Returns `0.0` when there is nothing valid to size.
pub fn sized_qty(closes: &[f64], price: f64) -> f64 {
    if price.is_finite() && price > 0.0 {
        TARGET_NOTIONAL * size_factor(closes) / price
    } else {
        0.0
    }
}

/// Realized-volatility sizing: annualization uses 5-minute bars
/// (12/h × 24h × 365d = 105 120 bars/year).
fn size_factor(closes: &[f64]) -> f64 {
    const WIN: usize = 20;
    if closes.len() < WIN + 1 {
        return 1.0;
    }
    let window = &closes[closes.len() - WIN - 1..];
    let rets: Vec<f64> = window
        .windows(2)
        .filter(|w| w[0] > 0.0 && w[1] > 0.0)
        .map(|w| (w[1] / w[0]).ln())
        .collect();
    if rets.len() < 2 {
        return 1.0;
    }
    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rets.len() as f64;
    let vol_ann = var.sqrt() * (105_120.0f64).sqrt();
    if !vol_ann.is_finite() || vol_ann <= 0.0 {
        return 1.0;
    }
    (0.5 / vol_ann).clamp(0.25, 1.0)
}

/// Run one deterministic orchestration cycle for `symbol` on `closes`
/// (oldest first, may be empty) under neutral market context.
///
/// Thin wrapper over [`run_cycle_with_context`]: price signals decide alone.
pub fn run_cycle_with_prices(symbol: &str, closes: &[f64]) -> Vec<AgentDecision> {
    run_cycle_with_context(symbol, closes, MarketContext::neutral())
}

/// Run one deterministic orchestration cycle for `symbol` on `closes`
/// (oldest first, may be empty) under `ctx`.
///
/// Empty history degrades every agent to hold (except the reporting
/// roles). With prices, technical votes SMA, strategy aggregates the
/// three votes (soft-vetoed by news beyond ±[`SENTIMENT_VETO`]), quant
/// sizes a [`TARGET_NOTIONAL`] order scaled by realized volatility,
/// risk approves or vetoes against conservative defaults, and execution
/// carries the validated intent the scheduler places. No network, no
/// secrets, no I/O.
pub fn run_cycle_with_context(
    symbol: &str,
    closes: &[f64],
    ctx: MarketContext,
) -> Vec<AgentDecision> {
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
    let price_signal = aggregate(&votes);
    // News soft-veto: headlines beyond ±SENTIMENT_VETO overrule the
    // price vote in their direction (bearish news blocks buys, bullish
    // news blocks sells). Funding soft-veto: an overcrowded perpetual
    // crowd (beyond ±FUNDING_VETO) blocks trades riding with it. Each
    // veto is logged in the reason.
    let (mut final_signal, mut vetoed) = match price_signal {
        Signal::Buy if ctx.sentiment <= -SENTIMENT_VETO => {
            (Signal::Hold, format!("news veto (sentiment {:.2})", ctx.sentiment))
        }
        Signal::Sell if ctx.sentiment >= SENTIMENT_VETO => {
            (Signal::Hold, format!("news veto (sentiment {:.2})", ctx.sentiment))
        }
        s => (s, String::new()),
    };
    if final_signal == Signal::Buy {
        if let Some(f) = ctx.funding {
            if f >= FUNDING_VETO {
                vetoed = format!("funding veto (rate {f:.4} overcrowded long)");
                final_signal = Signal::Hold;
            }
        }
    }
    if final_signal == Signal::Sell {
        if let Some(f) = ctx.funding {
            if f <= -FUNDING_VETO {
                vetoed = format!("funding veto (rate {f:.4} overcrowded short)");
                final_signal = Signal::Hold;
            }
        }
    }
    let voter_conf: f64 = if final_signal == Signal::Hold && price_signal != Signal::Hold {
        0.0
    } else {
        let voters: Vec<f64> = votes
            .iter()
            .filter(|v| v.signal == final_signal && final_signal != Signal::Hold)
            .map(|v| v.confidence)
            .collect();
        if voters.is_empty() {
            0.0
        } else {
            voters.iter().sum::<f64>() / voters.len() as f64
        }
    };
    let strategy_reason = if vetoed.is_empty() {
        format!(
            "{}:{} {}:{} {}:{} => {}",
            sma.strategy,
            sma.signal.action(),
            mr.strategy,
            mr.signal.action(),
            mom.strategy,
            mom.signal.action(),
            final_signal.action()
        )
    } else {
        format!(
            "{}:{} {}:{} {}:{} => price {} but {vetoed}",
            sma.strategy,
            sma.signal.action(),
            mr.strategy,
            mr.signal.action(),
            mom.strategy,
            mom.signal.action(),
            price_signal.action(),
        )
    };
    out.push(decide(
        "strategy",
        final_signal.action(),
        symbol,
        voter_conf,
        strategy_reason,
    ));

    // Quant: size a TARGET_NOTIONAL order at the last price, scaled
    // down when realized volatility is hot (factor in (0.25, 1.0]).
    let factor = size_factor(closes);
    let qty = last.map(|p| {
        if p > 0.0 {
            TARGET_NOTIONAL * factor / p
        } else {
            0.0
        }
    });
    match (last, qty) {
        (Some(p), Some(q)) if q > 0.0 && p.is_finite() => out.push(decide(
            "quant",
            "size",
            symbol,
            0.5,
            format!("qty {q:.6} for ${TARGET_NOTIONAL:.0}x{factor:.2} @ {p:.2}"),
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
/// `503`. Context is live (RSS/Fear&Greed sentiment + BTC funding);
/// when feeds are unreachable, degrades to the priceless all-hold
/// cycle instead of failing.
pub async fn run_cycle_handler(
    tenant: Tenant,
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentDecision>>, (StatusCode, String)> {
    let tenant_id: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid tenant".to_string()))?;
    let closes = live_closes().await.unwrap_or_default();
    let rss = crate::news::fetch_sentiment().await;
    let fg = crate::news::fetch_fear_greed().await;
    let funding = crate::news::fetch_funding("BTC").await;
    let ctx = MarketContext { sentiment: crate::news::combine_sentiment(rss, fg), funding };
    let decisions = run_cycle_with_context("BTC-USD", &closes, ctx);
    record_decisions(&state.pool, tenant_id, &decisions)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")))?;
    Ok(Json(decisions))
}

/// Candle granularity (seconds) for the live feed. Default 300 (5m,
/// historical behavior). The 1h research package runs at 3600:
/// `LIVE_GRANULARITY_SEC=3600`. Read live on every call so operators
/// retune without a restart.
pub fn live_granularity_sec() -> u64 {
    std::env::var("LIVE_GRANULARITY_SEC")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|g| (60..=86_400).contains(g))
        .unwrap_or(300)
}

/// Strategy feeding the scheduler's signal. Default `aggregate` (vote
/// of sma-cross + mean-reversion + momentum, historical behavior).
/// `mr` runs mean-reversion alone — the only family that measured
/// above 50% winrate out-of-sample on 1h data (55-70% across the
/// three products, fees aside). Read live on every call.
pub fn strategy_mode() -> String {
    std::env::var("STRATEGY_MODE").unwrap_or_else(|_| "aggregate".to_string())
}

/// Fetch recent closes for `symbol` from Coinbase, oldest first.
///
/// Granularity comes from [`live_granularity_sec`]; any failure
/// degrades to an empty series (the cycle then holds). Never panics,
/// never logs secrets.
pub async fn live_closes_for(symbol: &str) -> Result<Vec<f64>, String> {
    let client = crate::coinbase::CoinbaseClient::new();
    let granularity = live_granularity_sec();
    let candles = client
        .fetch_candles(symbol, granularity)
        .await
        .map_err(|e| format!("feed: {e}"))?;
    Ok(candles.iter().map(|c| c.close).collect())
}

/// Fetch recent BTC-USD 5-minute closes from Coinbase, oldest first.
///
/// Convenience wrapper over [`live_closes_for`] for the manual cycle endpoint.
pub async fn live_closes() -> Result<Vec<f64>, String> {
    live_closes_for("BTC-USD").await
}

/// Axum handler returning this cycle's decisions for `BTC-USD` as JSON.
///
/// Requires login ([`Tenant`]). Deterministic stub: no network, no
/// secrets, no I/O.
pub async fn agents_handler(_tenant: crate::state::Tenant) -> Json<Vec<AgentDecision>> {
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

    #[test]
    fn bearish_news_vetoes_buy_bullish_news_vetoes_sell() {
        let mut up = vec![100.0; 19];
        up.push(112.0); // price says buy
        let bearish = MarketContext { sentiment: -0.8, funding: None };
        let d = run_cycle_with_context("BTC-USD", &up, bearish);
        let by_agent = |id: &str| d.iter().find(|a| a.agent == id).unwrap();
        assert_eq!(by_agent("strategy").action, "hold");
        assert!(by_agent("strategy").reason.contains("news veto"));
        assert_eq!(by_agent("execution").action, "hold");

        let mut down = vec![112.0; 19];
        down.push(100.0); // price says sell
        let bullish = MarketContext { sentiment: 0.8, funding: None };
        let d = run_cycle_with_context("BTC-USD", &down, bullish);
        let by_agent = |id: &str| d.iter().find(|a| a.agent == id).unwrap();
        assert_eq!(by_agent("strategy").action, "hold");
        assert!(by_agent("strategy").reason.contains("news veto"));
    }

    #[test]
    fn mild_sentiment_does_not_veto() {
        let mut up = vec![100.0; 19];
        up.push(112.0);
        let mild = MarketContext { sentiment: -0.2, funding: None };
        let d = run_cycle_with_context("BTC-USD", &up, mild);
        assert_eq!(d.iter().find(|a| a.agent == "strategy").unwrap().action, "buy");
    }

    #[test]
    fn crowded_funding_vetoes_with_the_crowd() {
        let mut up = vec![100.0; 19];
        up.push(112.0); // price says buy
        let crowded_long = MarketContext { sentiment: 0.0, funding: Some(0.0009) };
        let d = run_cycle_with_context("BTC-USD", &up, crowded_long);
        let by_agent = |id: &str| d.iter().find(|a| a.agent == id).unwrap();
        assert_eq!(by_agent("strategy").action, "hold");
        assert!(by_agent("strategy").reason.contains("funding veto"));

        let mut down = vec![112.0; 19];
        down.push(100.0); // price says sell
        let crowded_short = MarketContext { sentiment: 0.0, funding: Some(-0.0009) };
        let d = run_cycle_with_context("BTC-USD", &down, crowded_short);
        let by_agent = |id: &str| d.iter().find(|a| a.agent == id).unwrap();
        assert_eq!(by_agent("strategy").action, "hold");
        assert!(by_agent("strategy").reason.contains("funding veto"));
    }

    #[test]
    fn normal_funding_does_not_veto() {
        let mut up = vec![100.0; 19];
        up.push(112.0);
        let calm = MarketContext { sentiment: 0.0, funding: Some(0.00009) };
        let d = run_cycle_with_context("BTC-USD", &up, calm);
        assert_eq!(d.iter().find(|a| a.agent == "strategy").unwrap().action, "buy");
    }

    #[test]
    fn size_factor_shrinks_when_volatile() {
        // Flat market: full size.
        assert!((size_factor(&vec![100.0; 25]) - 1.0).abs() < 1e-9);
        // Wild swings: minimum size.
        let mut wild = Vec::new();
        for i in 0..30 {
            wild.push(if i % 2 == 0 { 100.0 } else { 120.0 });
        }
        assert!(size_factor(&wild) < 0.5, "wild market must shrink size");
        // Too short: full size.
        assert_eq!(size_factor(&[100.0, 101.0]), 1.0);
    }

    #[tokio::test]
    async fn handler_returns_8() {
        let tenant = crate::state::Tenant {
            tenant_id: "00000000-0000-0000-0000-000000000001".to_string(),
            user_id: "u".to_string(),
        };
        let Json(decisions) = agents_handler(tenant).await;
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
