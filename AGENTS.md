# AGENTS.md — how to work in warden-trade

- One task, one file per worker call. Never commit from a worker.
- `cargo test` is the gate. Nix closure gate comes later.
- Raw SQL needs runtime proof (migrate + query against real Postgres).
- Never invent an exchange API, crate feature, or response shape. Check official docs first.
- Paper trading only. No live keys, no real funds, no withdraw scope.
- No secrets in code, git, logs, or templates. `.env` is gitignored; `.env.example` holds placeholders.
- Multi-tenancy from day one: every row carries `tenant_id`, every query filters on it.
- Commit per green step, first commit < 10 min.
