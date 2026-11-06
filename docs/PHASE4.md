# Phase 4 — Intelligence artificielle (delivered)

## Decisions
- Agents are deterministic stubs: `AgentId` / `AgentKind` (MarketAnalyst, StrategyEvaluator, RiskValidator, OrderExecutor) / `AgentDecision` (agent, action, symbol, confidence 0..1, reason, created_at).
- `evaluate_stub` always returns hold / 0.0 / stub. No model calls, no network, no secrets.
- Orchestrator `run_cycle(symbol)` fans out over the 4 kinds in enum order; `GET /api/agents` returns the BTC-USD cycle.
- `run_backtest` is pure: up-moves count as wins, PnL sums positive diffs.
- `agent_decisions` migration persists decisions per tenant (confidence check 0..1, index tenant+created_at).
- Risk Engine stays independent: agents propose, `check_order` disposes. No agent can touch limits or kill switch.

## Verification
- `cargo test`: 30 passed.
- Live: `/api/agents` 200 with 4 hold decisions.
- Migrations 0001+0002+0003 applied on temp PG, 5 tables confirmed.

## Files
- `src/agents.rs`, `src/orchestrator.rs`, `src/backtest.rs`, `migrations/0003_decisions.sql`, route wired.

## Next (Phase 5)
- Autonomy: scheduled paper cycles, load/resilience checks, risk-limit validation, security tests. Live only after explicit validation.
