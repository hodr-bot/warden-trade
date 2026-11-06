//! Agent roster: 8 specialised roles with structured I/O and decision history.
//!
//! Each [`AgentKind`] has a stable id, a precise role, and declared
//! input/output shapes ([`AgentInput`] in, [`AgentDecision`] out).
//! Decisions are persisted to `agent_decisions` by the orchestrator so
//! every run leaves history. Pure logic here: no network, no secrets, no I/O.

/// Newtype wrapper around an agent identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AgentId(pub String);

impl AgentId {
    /// Build a new [`AgentId`] from anything string-like.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The 8 specialised roles in the decision pipeline.
///
/// Order matters: [`run_cycle`](crate::orchestrator::run_cycle) runs them
/// in enum order, from market observation down to performance review.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    /// Reads live market snapshots (prices, venues).
    Market,
    /// Computes technical signals (trend, momentum) from price history.
    Technical,
    /// Selects and parameterises the strategy for the current regime.
    Strategy,
    /// Sizes positions and estimates cost/benefit (fees, slippage).
    Quant,
    /// Independent veto: validates every intent against risk limits.
    Risk,
    /// Turns validated intents into paper orders (paper only).
    Execution,
    /// Tracks portfolio state, exposure and drawdown.
    Portfolio,
    /// Reviews closed runs: win rate, PnL attribution, regime notes.
    Performance,
}

impl AgentKind {
    /// Stable machine id, also used as the `agent` column in history.
    pub fn id(self) -> &'static str {
        match self {
            AgentKind::Market => "market",
            AgentKind::Technical => "technical",
            AgentKind::Strategy => "strategy",
            AgentKind::Quant => "quant",
            AgentKind::Risk => "risk",
            AgentKind::Execution => "execution",
            AgentKind::Portfolio => "portfolio",
            AgentKind::Performance => "performance",
        }
    }

    /// One-line role contract: what this agent owns in the pipeline.
    pub fn role(self) -> &'static str {
        match self {
            AgentKind::Market => "observe live prices and report the current market state",
            AgentKind::Technical => "derive trend/momentum signals from price history",
            AgentKind::Strategy => "pick the strategy and parameters for the current regime",
            AgentKind::Quant => "size positions and estimate fees/slippage/drawdown",
            AgentKind::Risk => "veto any intent that breaches risk limits (independent)",
            AgentKind::Execution => "place validated paper orders, never real funds",
            AgentKind::Portfolio => "track exposure, drawdown and open positions",
            AgentKind::Performance => "review closed runs and attribute PnL per regime",
        }
    }
}

/// Structured input every agent receives for one cycle.
#[derive(Debug, Clone)]
pub struct AgentInput {
    /// Trading symbol, e.g. `"BTC-USD"`.
    pub symbol: String,
    /// Recent closing prices, oldest first (may be empty for market-only reads).
    pub closes: Vec<f64>,
}

impl AgentInput {
    /// Build an input for `symbol` with `closes` history attached.
    pub fn new(symbol: impl Into<String>, closes: Vec<f64>) -> Self {
        Self {
            symbol: symbol.into(),
            closes,
        }
    }
}

/// A single agent decision (persisted to `agent_decisions` by the orchestrator).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AgentDecision {
    /// Stable agent id (see [`AgentKind::id`]).
    pub agent: String,
    /// Action decided, e.g. `"hold"`.
    pub action: String,
    /// Trading symbol, e.g. `"BTC-USD"`.
    pub symbol: String,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Human-readable reason for the decision.
    pub reason: String,
    /// Creation timestamp as RFC3339 string (backend clock).
    pub created_at: String,
}

/// Pure stub evaluator: always returns a `hold` decision.
///
/// Deterministic: confidence is always `0.0`, reason is always `"stub"`,
/// and `created_at` is the current backend time. No network, no secrets, no I/O.
/// Real model-backed evaluation arrives with the `ModelProvider` step and
/// will keep this exact signature plus an [`AgentInput`].
pub fn evaluate_stub(agent: &AgentId, _kind: &AgentKind, symbol: &str) -> AgentDecision {
    AgentDecision {
        agent: agent.0.clone(),
        action: "hold".to_string(),
        symbol: symbol.to_string(),
        confidence: 0.0,
        reason: "stub".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> AgentId {
        AgentId::new("test-agent")
    }

    #[test]
    fn roster_has_8_distinct_ids_and_roles() {
        let kinds = [
            AgentKind::Market,
            AgentKind::Technical,
            AgentKind::Strategy,
            AgentKind::Quant,
            AgentKind::Risk,
            AgentKind::Execution,
            AgentKind::Portfolio,
            AgentKind::Performance,
        ];
        let mut ids: Vec<&str> = kinds.iter().map(|k| k.id()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 8, "each agent needs a distinct stable id");
        for kind in kinds {
            assert!(!kind.role().is_empty(), "{kind:?} needs a role contract");
        }
    }

    #[test]
    fn stub_returns_hold_action() {
        let decision = evaluate_stub(&test_agent(), &AgentKind::Market, "BTC-USD");
        assert_eq!(decision.action, "hold");
        assert_eq!(decision.agent, "test-agent");
        assert_eq!(decision.symbol, "BTC-USD");
        assert_eq!(decision.reason, "stub");
    }

    #[test]
    fn stub_confidence_in_range() {
        for kind in [
            AgentKind::Market,
            AgentKind::Technical,
            AgentKind::Strategy,
            AgentKind::Quant,
            AgentKind::Risk,
            AgentKind::Execution,
            AgentKind::Portfolio,
            AgentKind::Performance,
        ] {
            let decision = evaluate_stub(&test_agent(), &kind, "BTC-USD");
            assert!(
                (0.0..=1.0).contains(&decision.confidence),
                "confidence {} out of range for {kind:?}",
                decision.confidence
            );
            assert_eq!(decision.confidence, 0.0);
        }
    }

    #[test]
    fn input_carries_symbol_and_history() {
        let input = AgentInput::new("BTC-USD", vec![100.0, 101.0]);
        assert_eq!(input.symbol, "BTC-USD");
        assert_eq!(input.closes.len(), 2);
    }
}
