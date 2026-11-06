# STACK

- Backend: Rust stable (2021 edition), Axum 0.8, Tokio full, SQLx 0.8 (postgres, migrate, uuid, chrono, rust_decimal).
- DB: PostgreSQL 18, SQLx migrations in `migrations/`.
- Frontend: Askama templates + HTMX + custom CSS. No JS framework.
- Auth: Argon2 password hashing, JWT sessions (httpOnly cookies later), MFA placeholder for Phase 6.
- Config: `config` crate + `dotenvy`, `DATABASE_URL`, `BIND_ADDR`, `JWT_SECRET` (dev placeholder only).
- Observability: tracing + tower-http trace. OpenTelemetry evaluation in Phase 5/8.
