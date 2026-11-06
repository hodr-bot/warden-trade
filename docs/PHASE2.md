# Phase 2 — Exchanges (delivered)

## Decisions
- Public feeds only, read-only, no auth, no secrets: Coinbase v2 spot, Kraken public ticker.
- Shared `ExchangeError` taxonomy: Timeout / Network / RateLimited (+retry_after) / BadResponse / Auth / NotFound / Uncertain.
- Timeouts: 10s connect+read via reqwest + tokio deadline. No blind retry on uncertain outcomes.
- `paper_snapshot()` backs `GET /api/markets` until live wiring lands.

## Verification
- `cargo test`: 17 passed.
- Endpoints cited from worker-read code comments matching official public paths; full doc re-check due before live polling cadence lands.
- No secrets in repo; no network in unit tests except unreachable loopback.

## Files
- `src/exchanges.rs`, `src/coinbase.rs` (+fetch), `src/kraken.rs` (+fetch), `src/markets.rs`, route wired.

## Next (Phase 3)
- Deterministic Risk Engine: limits struct, check_order pure function, kill switch, paper ledger, reconciliation stub.
