//! Empirical calibration: size entries by measured edge, shrink-only.
//!
//! The bot used to trade a fixed size whether a signal was strong or weak.
//! This module measures what actually happened — per-symbol closed trades
//! from [`paper_orders`](crate::orders) — and returns a size multiplier:
//!
//! - fewer than [`MIN_CLOSES`] closes: `1.0` (not enough data, keep
//!   learning at full size);
//! - negative expectancy: `0.25` (proven loser: trade tiny, keep observing,
//!   never freeze completely or learning stops);
//! - positive expectancy: `0.5`–`1.0`, scaled by expectancy relative to the
//!   average win.
//!
//! Shrink-only by construction: calibration can never inflate an order
//! above the base size from [`crate::orchestrator::sized_qty`]. It applies
//! to entries (buys) only — exits and signal sells always close the full
//! position, otherwise a loser could never be fully flattened.

/// Closes needed before the multiplier may move below 1.0.
pub const MIN_CLOSES: u32 = 10;
/// Floor multiplier for negative-expectancy symbols.
pub const FLOOR_MULT: f64 = 0.25;

/// One closed round-trip: net PnL of a single sell order matched FIFO
/// against earlier buys. Positive = win.
#[derive(Debug, Clone, PartialEq)]
pub struct CloseOutcome {
    pub pnl: f64,
}

/// Fold raw fills `(side, qty, price)`, oldest first, into per-sell
/// outcomes. Buys open lots; each sell closes lots FIFO and records ONE
/// outcome for the whole order (not one per lot, so a single sale cannot
/// inflate the sample count).
pub fn close_outcomes(fills: &[(String, f64, f64)]) -> Vec<CloseOutcome> {
    let mut lots: Vec<(f64, f64)> = Vec::new();
    let mut out = Vec::new();
    for (side, qty, price) in fills {
        if side == "buy" {
            lots.push((*qty, *price));
            continue;
        }
        let mut rest = *qty;
        let mut pnl = 0.0;
        let mut matched = false;
        while rest > 0.0 {
            match lots.first_mut() {
                Some((q, p)) => {
                    let m = rest.min(*q);
                    pnl += (*price - *p) * m;
                    *q -= m;
                    rest -= m;
                    matched = true;
                    if *q <= 0.0 {
                        lots.remove(0);
                    }
                }
                None => break, // short sale: no buy to close, no outcome
            }
        }
        if matched {
            out.push(CloseOutcome { pnl });
        }
    }
    out
}

/// Summary statistics over closed outcomes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeStats {
    pub closes: u32,
    pub wins: u32,
    pub avg_win: f64,
    pub avg_loss: f64,
}

impl EdgeStats {
    pub fn from_outcomes(outcomes: &[CloseOutcome]) -> Self {
        let (mut wins, mut sum_w, mut sum_l) = (0u32, 0.0, 0.0);
        for o in outcomes {
            if o.pnl > 0.0 {
                wins += 1;
                sum_w += o.pnl;
            } else {
                sum_l -= o.pnl;
            }
        }
        let losses = outcomes.len() as u32 - wins;
        EdgeStats {
            closes: outcomes.len() as u32,
            wins,
            avg_win: if wins > 0 { sum_w / wins as f64 } else { 0.0 },
            avg_loss: if losses > 0 { sum_l / losses as f64 } else { 0.0 },
        }
    }

    /// Expectancy per unit traded: `p*W - q*L`.
    pub fn expectancy(&self) -> f64 {
        if self.closes == 0 {
            return 0.0;
        }
        let p = self.wins as f64 / self.closes as f64;
        p * self.avg_win - (1.0 - p) * self.avg_loss
    }
}

/// Size multiplier for the next entry. Pure; see module docs.
pub fn multiplier(stats: &EdgeStats) -> f64 {
    if stats.closes < MIN_CLOSES {
        return 1.0;
    }
    let e = stats.expectancy();
    if e <= 0.0 || stats.avg_win <= 0.0 {
        return FLOOR_MULT;
    }
    (0.5 + e / (2.0 * stats.avg_win)).clamp(FLOOR_MULT, 1.0)
}

/// One-line human summary for tick logs (`win 42% over 31 closes, x0.50`).
pub fn describe(stats: &EdgeStats) -> String {
    if stats.closes == 0 {
        return "no closes yet".to_string();
    }
    let pct = stats.wins * 100 / stats.closes;
    format!(
        "win {pct}% over {} closes, x{:.2}",
        stats.closes,
        multiplier(stats)
    )
}

/// Load recent fills for one symbol and compute its edge. Thin SQL only;
/// all math stays in the pure functions above (unit-tested without a DB).
pub async fn calibration_for(
    pool: &sqlx::PgPool,
    tenant_id: uuid::Uuid,
    symbol: &str,
) -> EdgeStats {
    let fills: Vec<(String, f64, f64)> = sqlx::query_as(
        "SELECT side, qty, price FROM paper_orders WHERE tenant_id = $1 AND symbol = $2 ORDER BY created_at ASC LIMIT 500",
    )
    .bind(tenant_id)
    .bind(symbol)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    EdgeStats::from_outcomes(&close_outcomes(&fills))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fills(raw: &[(&str, f64, f64)]) -> Vec<(String, f64, f64)> {
        raw.iter()
            .map(|(s, q, p)| (s.to_string(), *q, *p))
            .collect()
    }

    #[test]
    fn one_sell_is_one_outcome_even_across_lots() {
        // Two buy lots, one sell eating both: a single close.
        let f = fills(&[("buy", 1.0, 100.0), ("buy", 1.0, 110.0), ("sell", 2.0, 120.0)]);
        let out = close_outcomes(&f);
        assert_eq!(out.len(), 1);
        assert!((out[0].pnl - 30.0).abs() < 1e-9, "pnl={}", out[0].pnl);
    }

    #[test]
    fn short_sale_records_no_outcome() {
        let f = fills(&[("sell", 1.0, 100.0)]);
        assert!(close_outcomes(&f).is_empty());
    }

    #[test]
    fn thin_history_trades_full_size() {
        let stats = EdgeStats {
            closes: 9,
            wins: 0,
            avg_win: 0.0,
            avg_loss: 5.0,
        };
        assert_eq!(multiplier(&stats), 1.0);
    }

    #[test]
    fn negative_expectancy_hits_floor() {
        // 40% wins, 1:1 payoff -> expectancy < 0.
        let mut outcomes = Vec::new();
        for _ in 0..4 {
            outcomes.push(CloseOutcome { pnl: 10.0 });
        }
        for _ in 0..6 {
            outcomes.push(CloseOutcome { pnl: -10.0 });
        }
        let stats = EdgeStats::from_outcomes(&outcomes);
        assert_eq!(stats.closes, 10);
        assert!(stats.expectancy() < 0.0);
        assert_eq!(multiplier(&stats), FLOOR_MULT);
    }

    #[test]
    fn strong_edge_keeps_full_size() {
        // 80% wins at 1:1 -> e = 0.6W -> 0.5+0.3 = 0.8.
        let mut outcomes = Vec::new();
        for _ in 0..8 {
            outcomes.push(CloseOutcome { pnl: 10.0 });
        }
        for _ in 0..2 {
            outcomes.push(CloseOutcome { pnl: -10.0 });
        }
        let stats = EdgeStats::from_outcomes(&outcomes);
        assert!((multiplier(&stats) - 0.8).abs() < 1e-9);
    }

    #[test]
    fn describe_is_stable() {
        assert_eq!(describe(&EdgeStats::from_outcomes(&[])), "no closes yet");
        let wins10: Vec<CloseOutcome> = (0..10).map(|_| CloseOutcome { pnl: 1.0 }).collect();
        let stats = EdgeStats::from_outcomes(&wins10);
        assert!(describe(&stats).starts_with("win 100% over 10 closes"));
    }

    /// DB-gated: needs a real Postgres (`WARDEN_TEST_DATABASE_URL`).
    /// Proves the SQL reader end to end; skipped in hermetic runs.
    #[tokio::test]
    async fn db_reader_counts_real_closes() {
        let url = match std::env::var("WARDEN_TEST_DATABASE_URL") {
            Ok(u) => u,
            Err(_) => return,
        };
        let pool = sqlx::PgPool::connect(&url).await.expect("proof pg");
        let tenant: uuid::Uuid = sqlx::query_scalar(
            "INSERT INTO tenants (name) VALUES ('cal-proof') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("tenant");
        for (side, qty, price) in [
            ("buy", 1.0, 100.0),
            ("buy", 1.0, 100.0),
            ("sell", 2.0, 110.0),
        ] {
            sqlx::query(
                "INSERT INTO paper_orders (tenant_id, exchange, symbol, side, qty, price) VALUES ($1,'coinbase','CAL-USD',$2,$3,$4)",
            )
            .bind(tenant)
            .bind(side)
            .bind(qty)
            .bind(price)
            .execute(&pool)
            .await
            .expect("order");
        }
        let stats = calibration_for(&pool, tenant, "CAL-USD").await;
        assert_eq!(stats.closes, 1);
        assert_eq!(stats.wins, 1);
        assert!((multiplier(&stats) - 1.0).abs() < 1e-9); // thin history
        sqlx::query("DELETE FROM tenants WHERE id = $1")
            .bind(tenant)
            .execute(&pool)
            .await
            .ok();
    }
}
