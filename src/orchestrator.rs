//! Orchestrator cycle: deterministic fan-out over the 8-agent roster.
//!
//! [`run_cycle`] is pure (no network, no secrets, no I/O) and calls
//! [`crate::agents::evaluate_stub`] once per [`crate::agents::AgentKind`].
//! [`run_cycle_handler`] (`POST /api/agents/run`) executes the same cycle
//! for the authenticated tenant and persists every decision to
//! `agent_decisions`, so history feeds `GET /api/reconcile`.

use axum::{extract::State, http::StatusCode, Json};

use crate::agents::{evaluate_stub, AgentDecision, AgentId, AgentKind};
use crate::state::{AppState, Tenant};

/// Stable stub agent id for each kind (see [`AgentKind::id`]).
fn agent_id_for(kind: &AgentKind) -> AgentId {
    AgentId::new(kind.id())
}

/// Run one deterministic orchestration cycle for `symbol`.
///
/// Calls [`evaluate_stub`](crate::agents::evaluate_stub) once for each of
/// the 8 [`AgentKind`] variants, in enum order, and returns the 8 decisions.
/// No network, no secrets, no I/O.
pub fn run_cycle(symbol: &str) -> Vec<AgentDecision> {
    const KINDS: [AgentKind; 8] = [
        AgentKind::Market,
        AgentKind::Technical,
        AgentKind::Strategy,
        AgentKind::Quant,
        AgentKind::Risk,
        AgentKind::Execution,
        AgentKind::Portfolio,
        AgentKind::Performance,
    ];
    KINDS
        .iter()
        .map(|kind| {
            let id = agent_id_for(kind);
            evaluate_stub(&id, kind, symbol)
        })
        .collect()
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

/// `POST /api/agents/run`: execute one cycle for the authenticated tenant
/// and persist the 8 decisions.
///
/// Requires `Authorization: Bearer *** via [`Tenant`]. DB failures become
/// `503`. The cycle itself stays deterministic and paper-only.
pub async fn run_cycle_handler(
    tenant: Tenant,
    State(state): State<AppState>,
) -> Result<Json<Vec<AgentDecision>>, (StatusCode, String)> {
    let tenant_id: uuid::Uuid = tenant
        .tenant_id
        .parse()
        .map_err(|_| (StatusCode::BAD_REQUEST, "invalid tenant".to_string()))?;
    let decisions = run_cycle("BTC-USD");
    record_decisions(&state.pool, tenant_id, &decisions)
        .await
        .map_err(|e| (StatusCode::SERVICE_UNAVAILABLE, format!("db: {e}")))?;
    Ok(Json(decisions))
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
        for d in &decisions {
            assert_eq!(d.symbol, "BTC-USD");
            assert_eq!(d.action, "hold");
        }
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
