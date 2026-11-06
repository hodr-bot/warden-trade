# Phase 1 — Foundations (delivered)

## Decisions
- Rust 2021 edition, Axum 0.8, Tokio full, SQLx 0.8 (postgres, migrate, uuid, chrono, rust_decimal).
- Askama 0.12 + HTMX + custom CSS. No React/Next/TS. tower-http `fs` for `/static`.
- Auth: Argon2 default PHC + JWT HS256 claims (`sub` user_id, `tenant_id`, `exp`). MFA deferred to Phase 6.
- Multi-tenancy: `tenants` / `users` (unique per tenant) / `sessions`; every future table carries `tenant_id`.
- Placeholders only in repo (`.env.example`). Real `DATABASE_URL` / `JWT_SECRET` via env.
- Paper trading only. No live keys, no withdraw scope, no real funds.

## Files
- `migrations/0001_init.sql` — tenants, users, sessions + indexes (runtime-proofed on temp PG 5544).
- `src/auth.rs` — hash/verify, JWT encode/decode, 3 tests.
- `src/state.rs` — AppState pool + secret, Tenant Bearer extractor, 2 tests.
- `src/dashboard.rs` + `templates/dashboard.html` — Askama dashboard, backend timestamp, 1 test.
- `static/css/design.css` — dark premium design system (917 lines).
- `src/main.rs` — `/health`, `/`, `/static`.

## Verification
- `cargo test`: 7 passed.
- `cargo build`: ok (9 dead-code warnings: auth/state not yet consumed by routes beyond dashboard).
- HTTP: `/` 200 with backend timestamp, `/health` ok.
- Secrets: no private keys, no live tokens in git.

## Next (Phase 2)
- Exchange trait + Coinbase + Kraken public market feeds (no trading yet).
- Account sync reads (paper keys, read-only).
- Error taxonomy: timeouts, retry/backoff, rate limits.
