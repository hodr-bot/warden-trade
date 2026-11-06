# Phase 5 — Autonomie (delivered, paper only)

## Decisions
- `validate_limits` gates every cycle: all four bounds must be > 0, else the report flags `limits_ok: false`.
- `run_paper_cycle` aggregates decisions + backtest + limits into one `AutonomyReport`; `GET /api/autonomy` serves it with stub prices.
- No scheduler daemon yet: cycles run on demand via the route. A cron/Temporal trigger lands only with explicit approval.
- Live trading stays DISABLED: no real-funds path exists in the codebase. Passage au réel exige validation explicite de Michel.

## Verification
- `cargo test`: 33 passed. `cargo build`: clean.
- Live: `/api/autonomy` 200 → 4 decisions, 2 wins, pnl 2.5, limits_ok true.
- `grep -rni 'BEGIN.*PRIVATE\|sk-live' src/ templates/ migrations/ static/`: clean.

## Files
- `src/autonomy.rs`, route wired.

## Next (Phase 6)
- SaaS admin: tenant/user management routes. Billing/facturation stays OUT of scope until MiCA clearance. No commercialisation before professional MiCA review.
