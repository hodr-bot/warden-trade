//! Orchestrator cycle: deterministic, offline-only fan-out over agent stubs.
//!
//! Pure logic only: no network, no secrets, no I/O. [`run_cycle`] calls
//! [`crate::agents::evaluate_stub`] once per [`crate::agents::AgentKind`]
//! variant and collects the four [`crate::agents::AgentDecision`]s.

use axum::Json;

use crate::agents::{evaluate_stub, AgentDecision, AgentId, AgentKind};

/// Stable stub agent id for each kind (deterministic, no I/O).
fn agent_id_for(kind: &AgentKind) -> AgentId {
    AgentId::new(match kind {
        AgentKind::MarketAnalyst => "market-analyst",
        AgentKind::StrategyEvaluator => "strategy-evaluator",
        AgentKind::RiskValidator => "risk-validator",
        AgentKind::OrderExecutor => "order-executor",
    })
}

/// Run one deterministic orchestration cycle for `symbol`.
///
/// Calls [`evaluate_stub`](crate::agents::evaluate_stub) once for each of
/// the 4 [`AgentKind`] variants, in enum order, and returns the 4 decisions.
/// No network, no secrets, deterministic only.
pub fn run_cycle(symbol: &str) -> Vec<AgentDecision> {
    const KINDS: [AgentKind; 4] = [
        AgentKind::MarketAnalyst,
        AgentKind::StrategyEvaluator,
        AgentKind::RiskValidator,
        AgentKind::OrderExecutor,
    ];
    KINDS
        .iter()
        .map(|kind| {
            let id = agent_id_for(kind);
            evaluate_stub(&id, kind, symbol)
        })
        .collect()
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
    fn cycle_returns_4_decisions() {
        let decisions = run_cycle("BTC-USD");
        assert_eq!(decisions.len(), 4, "run_cycle must return 4 decisions");
        for d in &decisions {
            assert_eq!(d.symbol, "BTC-USD");
            assert_eq!(d.action, "hold");
        }
        let agents: Vec<&str> = decisions.iter().map(|d| d.agent.as_str()).collect();
        assert_eq!(
            agents,
            vec![
                "market-analyst",
                "strategy-evaluator",
                "risk-validator",
                "order-executor"
            ]
        );
    }

    #[tokio::test]
    async fn handler_returns_4() {
        let Json(decisions) = agents_handler().await;
        assert_eq!(decisions.len(), 4, "handler must return 4 decisions");
        for d in &decisions {
            assert_eq!(d.symbol, "BTC-USD");
        }
        // Handler output must be JSON-serializable (proves Serialize impl works).
        let v = serde_json::to_value(&decisions).expect("decisions must serialize");
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap().len(), 4);
    }
}
