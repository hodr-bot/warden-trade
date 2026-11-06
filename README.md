# Warden Trade

Autonomous crypto trading SaaS, multi-exchange, AI-orchestrated. Solo-admin first.

## Non-negotiables
- Rust stable, Axum + Tokio + SQLx + PostgreSQL.
- Server-rendered HTML (Askama + HTMX). No React/Next/TS.
- Paper trading only until explicit live validation. Never use real funds in dev.
- Deterministic Risk Engine independent from AI agents. Agents never change limits.
- Secrets encrypted at rest, never in git/logs/frontend. Exchange keys withdraw-disabled, minimal scope.
- Compilable + tested at every step. Docs verified against official sources.
- MiCA review by a qualified professional before any commercialisation or third-party fund handling.

## Roadmap
- Phase 1 — Foundations: Axum + Postgres, auth, multi-tenancy, design system, dashboard.
- Phase 2 — Exchanges: Coinbase + Kraken, market feeds, account sync.
- Phase 3 — Trading: order engine, Risk Engine, paper trading, reconciliation.
- Phase 4 — AI: orchestrator, agents, backtesting, metrics.
- Phase 5 — Autonomy: simulated auto-exec, load/resilience, security tests.
- Phase 6 — SaaS: users, billing (later), admin, prod deploy.

## Run
- `cargo test`
- `cargo run` → http://127.0.0.1:3000/ + /health
- `DATABASE_URL=postgres://... cargo run`
