# Phase 3 — Trading (delivered)

## Decisions
- Risk Engine is a pure deterministic function (`check_order`), independent of any agent. Agents can never modify limits.
- Kill switch is a hard gate: when set, every order is rejected before any other check.
- Paper only: in-memory `PaperLedger`, every placement gated by `check_order`. No real funds path exists.
- `paper_orders` migration: tenant FK cascade, qty/price checks, UNIQUE(tenant, idempotency_key) against blind re-execution, index (tenant, created_at).
- Routes: `GET /api/orders` (JSON, empty until persistence lands), `GET /api/markets`, `/`, `/health`.

## Verification
- `cargo test`: 24 passed.
- Live: `/` 200, `/api/orders` 200 `[]`, `/api/markets` 200.
- Migrations 0001+0002 applied on temp PG, `\d paper_orders` confirmed.
- `grep -rni 'BEGIN.*PRIVATE\|sk-live' src/ templates/ migrations/` clean.

## Files
- `src/risk.rs`, `src/paper.rs`, `src/orders.rs`, `migrations/0002_orders.sql`, routes wired.

## Next (Phase 4)
- Orchestrator + agent contracts (message schema, decision log), backtest stub, strategy evaluation stub. No live trading.
