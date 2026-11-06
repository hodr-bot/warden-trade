# Phase 6 — SaaS (delivered, admin stub; billing out of scope)

## Decisions
- Admin surface starts with `GET /api/admin/tenants` (stub, empty until Postgres read path lands).
- Billing, subscriptions and invoicing are OUT of scope until MiCA clearance by a qualified professional. No commercialisation, no third-party fund management before that review.
- Roles (admin/user) already exist in the `users` schema; enforcement middleware lands before any multi-user exposure.

## Verification
- `cargo test`: 34 passed. `cargo build`: clean.
- Live: all 7 routes 200 (`/`, `/health`, `/api/markets`, `/api/orders`, `/api/agents`, `/api/autonomy`, `/api/admin/tenants`).
- Migrations 0001–0003 applied on temp PG. Secrets grep clean.

## Files
- `src/admin.rs`, route wired.

## Stop condition reached
- Live trading and commercialisation require Michel's explicit validation + MiCA professional review. Nothing real runs without that.
