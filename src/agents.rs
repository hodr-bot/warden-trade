//! Agent stubs: deterministic, offline-only decision types.
//!
//! Pure logic only: no network, no secrets, no I/O.

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

/// The kind of agent participating in the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    MarketAnalyst,
    StrategyEvaluator,
    RiskValidator,
    OrderExecutor,
}

/// A single agent decision (stub shape).
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDecision {
    /// Agent identifier that produced the decision.
    pub agent: String,
    /// Action decided, e.g. `"hold"`.
    pub action: String,
    /// Trading symbol, e.g. `"BTC-USD"`.
    pub symbol: String,
    /// Confidence in `[0.0, 1.0]`.
    pub confidence: f64,
    /// Human-readable reason for the decision.
    pub reason: String,
    /// Creation timestamp as RFC3339 string (fixed stub value).
    pub created_at: String,
}

/// Pure stub evaluator: always returns a `hold` decision.
///
/// Deterministic: confidence is always `0.0`, reason is always `"stub"`,
/// and `created_at` is a fixed constant. No network, no secrets, no I/O.
pub fn evaluate_stub(agent: &AgentId, _kind: &AgentKind, symbol: &str) -> AgentDecision {
    AgentDecision {
        agent: agent.0.clone(),
        action: "hold".to_string(),
        symbol: symbol.to_string(),
        confidence: 0.0,
        reason: "stub".to_string(),
        created_at: "1970-01-01T00:00:00Z".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_agent() -> AgentId {
        AgentId::new("test-agent")
    }

    #[test]
    fn stub_returns_hold_action() {
        let decision = evaluate_stub(&test_agent(), &AgentKind::MarketAnalyst, "BTC-USD");
        assert_eq!(decision.action, "hold");
        assert_eq!(decision.agent, "test-agent");
        assert_eq!(decision.symbol, "BTC-USD");
        assert_eq!(decision.reason, "stub");
    }

    #[test]
    fn stub_confidence_in_range() {
        for kind in [
            AgentKind::MarketAnalyst,
            AgentKind::StrategyEvaluator,
            AgentKind::RiskValidator,
            AgentKind::OrderExecutor,
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
}
