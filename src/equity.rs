//! Portfolio timeline: realized-PnL curve per period (paper only).
//!
//! Rebuilds cumulative realized PnL from `paper_orders` fills with the same
//! FIFO rule as the dashboard, then rebases to zero at the period start so
//! each tab answers "what did this period earn?". Unrealized (latent) value
//! is excluded on purpose: it needs mark history we do not store, and the
//! card label says so. Tenant-scoped like everything else.
//!
//! The curve is rendered server-side as inline SVG (no JS chart, no new
//! dependency): it rides the normal dashboard render and its 30 s refresh.

/// One step of the rebased curve: epoch seconds, period PnL at that trade,
/// running count of closing sells in the period.
#[derive(Debug, Clone, PartialEq)]
pub struct EquityPoint {
    pub t: i64,
    pub v: f64,
    pub trades: u32,
}

/// Canonical period ids (`?range=` on `/`). Unknown values fall back to
/// `week`: a bad query string shows data, never an error page.
pub fn normalize_range(raw: Option<&str>) -> &'static str {
    match raw {
        Some("hour") => "hour",
        Some("day") => "day",
        Some("month") => "month",
        Some("all") => "all",
        _ => "week",
    }
}

/// Period start as epoch seconds. `day` is local-midnight UTC, not "last
/// 24 h": the tab answers "today", matching its label.
pub fn range_start(range: &str, now_ts: i64) -> i64 {
    match range {
        "hour" => now_ts - 3_600,
        "day" => now_ts - now_ts.rem_euclid(86_400),
        "month" => now_ts - 30 * 86_400,
        "all" => 0,
        _ => now_ts - 7 * 86_400, // "week"
    }
}

/// FIFO realized-PnL curve over `(symbol, side, qty, price, ts)` fills,
/// oldest first, rebased to zero at `since_ts`.
///
/// Sells before the period establish the cost basis but contribute no
/// point; each sell inside the period appends one point. A sell with no
/// prior buy contributes nothing (same rule as the dashboard FIFO).
/// Returns `(points, period_realized, period_closing_trades)`.
pub fn equity_curve(
    fills: &[(String, String, f64, f64, i64)],
    since_ts: i64,
) -> (Vec<EquityPoint>, f64, u32) {
    use std::collections::BTreeMap;
    let mut lots: BTreeMap<String, Vec<(f64, f64)>> = BTreeMap::new();
    let (mut cum, mut base) = (0.0f64, None::<f64>);
    let (mut points, mut trades) = (Vec::new(), 0u32);
    for (symbol, side, qty, price, ts) in fills {
        if side == "buy" {
            lots.entry(symbol.clone()).or_default().push((*qty, *price));
        } else if side == "sell" {
            let mut rest = *qty;
            if let Some(q) = lots.get_mut(symbol) {
                while rest > 0.0 {
                    match q.first_mut() {
                        Some((lq, lp)) => {
                            let m = rest.min(*lq);
                            // First in-period sell freezes the baseline:
                            // earlier sells built the basis, not the curve.
                            if *ts >= since_ts && base.is_none() {
                                base = Some(cum);
                            }
                            cum += (*price - *lp) * m;
                            *lq -= m;
                            rest -= m;
                            if *lq <= 0.0 {
                                q.remove(0);
                            }
                        }
                        None => break,
                    }
                }
            }
            if *ts >= since_ts {
                trades += 1;
                points.push(EquityPoint {
                    t: *ts,
                    v: cum - base.unwrap_or(cum),
                    trades,
                });
            }
        }
    }
    let period = points.last().map(|p| p.v).unwrap_or(0.0);
    (points, period, trades)
}

/// Short UTC label for a trade dot tooltip (`11-09 08:40 UTC`). Pure.
fn fmt_point_ts(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0)
        .map(|dt| dt.format("%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| t.to_string())
}

/// Inline SVG step-line for the curve. Pure numbers in, markup out: stroke
/// follows the period sign via `currentColor` (the wrapper carries
/// `up`/`down`). Empty periods render a flat zero line plus a note.
///
/// When `live` is set, the last point is the live open value (realized +
/// unrealized at the render marks), drawn as a distinct `eq-live` dot
/// tagged `data-live="1"` — so an hour or day with no closes yet still
/// shows where the open book stands instead of a flat zero.
pub fn equity_svg(points: &[EquityPoint], since_ts: i64, now_ts: i64, live: bool) -> String {
    const W: f64 = 600.0;
    const H: f64 = 180.0;
    const PAD_L: f64 = 8.0;
    const PAD_R: f64 = 8.0;
    const PAD_T: f64 = 14.0;
    const PAD_B: f64 = 20.0;
    let t1 = now_ts.max(points.last().map(|p| p.t).unwrap_or(now_ts));
    let span = (t1 - since_ts).max(1) as f64;
    let x = |t: i64| PAD_L + ((t - since_ts) as f64 / span) * (W - PAD_L - PAD_R);
    let (mut lo, mut hi) = (0.0f64, 0.0f64);
    for p in points {
        lo = lo.min(p.v);
        hi = hi.max(p.v);
    }
    if (hi - lo).abs() < 1e-9 {
        lo -= 1.0;
        hi += 1.0;
    }
    let y = |v: f64| PAD_T + (1.0 - (v - lo) / (hi - lo)) * (H - PAD_T - PAD_B);
    let mut d = format!("M {:.1} {:.1}", x(since_ts), y(0.0));
    let mut prev_v = 0.0;
    for p in points {
        // Step: horizontal lead-in at the previous level, then vertical.
        d.push_str(&format!(" L {:.1} {:.1} L {:.1} {:.1}", x(p.t), y(prev_v), x(p.t), y(p.v)));
        prev_v = p.v;
    }
    // Extend flat to the right edge (now).
    if let Some(last) = points.last() {
        d.push_str(&format!(" L {:.1} {:.1}", x(t1), y(last.v)));
    } else {
        d.push_str(&format!(" L {:.1} {:.1}", x(t1), y(0.0)));
    }
    let zero = if lo < 0.0 && hi > 0.0 {
        format!(
            "<line x1=\"{:.1}\" y1=\"{:.1}\" x2=\"{:.1}\" y2=\"{:.1}\" class=\"eq-zero\"/>",
            PAD_L,
            y(0.0),
            W - PAD_R,
            y(0.0)
        )
    } else {
        String::new()
    };
    let end_dot = match points.last() {
        Some(last) => {
            let cls = if live { "eq-dot eq-live" } else { "eq-dot" };
            let live_attr = if live { " data-live=\"1\"" } else { "" };
            let prefix = if live { "live " } else { "" };
            format!(
                "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"2.5\" class=\"{cls}\" data-t=\"{}\" data-v=\"{:.2}\" data-n=\"{}\"{live_attr}><title>{}{} · {:+.2}</title></circle>",
                x(last.t),
                y(last.v),
                last.t,
                last.v,
                last.trades,
                prefix,
                fmt_point_ts(last.t),
                last.v
            )
        }
        None => String::new(),
    };
    // Trade dots (decimated to ~24 max): native hover tooltips plus a
    // tap-to-inspect readout wired in live.js (delegated, survives HTMX
    // swaps). The last point is covered by the end dot above.
    let step = ((points.len().max(1) + 23) / 24).max(1);
    let mut dots = String::new();
    let mut i = 0;
    while i + 1 < points.len() {
        let p = &points[i];
        dots.push_str(&format!(
            "<circle cx=\"{:.1}\" cy=\"{:.1}\" r=\"3\" class=\"eq-dot eq-trade\" data-t=\"{}\" data-v=\"{:.2}\" data-n=\"{}\"><title>{} · {:+.2}</title></circle>",
            x(p.t),
            y(p.v),
            p.t,
            p.v,
            p.trades,
            fmt_point_ts(p.t),
            p.v
        ));
        i += step;
    }
    format!(
        "<svg viewBox=\"0 0 {W} {H}\" role=\"img\" aria-label=\"Realized PnL curve\">\
         {zero}<path d=\"{d}\" class=\"eq-line\" fill=\"none\"/>{dots}{end_dot}\
         <text x=\"{:.1}\" y=\"{:.1}\" class=\"eq-label\">{:+.2}</text>\
         <text x=\"{:.1}\" y=\"{:.1}\" class=\"eq-label\" text-anchor=\"end\">{:+.2}</text>\
         </svg>",
        PAD_L + 2.0,
        PAD_T - 4.0,
        hi,
        W - PAD_R - 2.0,
        H - 6.0,
        lo,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dots_are_decimated_titled_and_data_tagged() {
        let pts: Vec<EquityPoint> = (0..50)
            .map(|i| EquityPoint { t: 1000 + i * 60, v: i as f64 * 0.1, trades: i as u32 + 1 })
            .collect();
        let svg = equity_svg(&pts, 900, 5000, false);
        let circles = svg.match_indices("<circle").count();
        assert!(circles <= 26, "decimated, got {circles}");
        assert!(circles >= 2, "trade dots plus end dot");
        // Every dot carries a native tooltip and machine-readable data.
        assert_eq!(svg.match_indices("<title>").count(), circles);
        assert!(svg.contains("data-t=\"1000\""));
        assert!(svg.contains("data-n=\"1\""));
        assert!(svg.contains("UTC"));
    }

    #[test]
    fn live_end_dot_is_tagged() {
        let pts = vec![EquityPoint { t: 1100, v: 10.0, trades: 1 }];
        let svg = equity_svg(&pts, 900, 1200, true);
        assert!(svg.contains("eq-live"));
        assert!(svg.contains("data-live=\"1\""));
        assert!(svg.contains("live "));
    }

    #[test]
    fn normalize_range_falls_back_to_week() {
        assert_eq!(normalize_range(None), "week");
        assert_eq!(normalize_range(Some("day")), "day");
        assert_eq!(normalize_range(Some("nope")), "week");
    }

    #[test]
    fn range_start_pins_periods() {
        // 2026-11-08 12:00 UTC.
        let now = 1794139200;
        assert_eq!(range_start("hour", now), now - 3600);
        assert_eq!(range_start("day", now), 1794096000);
        assert_eq!(range_start("week", now), now - 7 * 86400);
        assert_eq!(range_start("month", now), now - 30 * 86400);
        assert_eq!(range_start("all", now), 0);
    }

    fn fill(sym: &str, side: &str, qty: f64, price: f64, ts: i64) -> (String, String, f64, f64, i64) {
        (sym.to_string(), side.to_string(), qty, price, ts)
    }

    #[test]
    fn curve_rebases_at_period_start() {
        // Buy 1 @ 100 (t=100), sell 1 @ 150 (t=200, before), buy 1 @ 100
        // (t=1100), sell 1 @ 120 (t=1200, inside since=1000).
        let fills = vec![
            fill("BTC-USD", "buy", 1.0, 100.0, 100),
            fill("BTC-USD", "sell", 1.0, 150.0, 200),
            fill("BTC-USD", "buy", 1.0, 100.0, 1100),
            fill("BTC-USD", "sell", 1.0, 120.0, 1200),
        ];
        let (pts, period, trades) = equity_curve(&fills, 1000);
        assert_eq!(trades, 1);
        assert_eq!(pts.len(), 1);
        assert!((period - 20.0).abs() < 1e-9, "got {period}");
        assert!((pts[0].v - 20.0).abs() < 1e-9);
    }

    #[test]
    fn curve_symbols_stay_independent() {
        let fills = vec![
            fill("BTC-USD", "buy", 1.0, 100.0, 1100),
            fill("ETH-USD", "buy", 1.0, 50.0, 1100),
            fill("BTC-USD", "sell", 1.0, 130.0, 1200),
            fill("ETH-USD", "sell", 1.0, 40.0, 1300),
        ];
        let (pts, period, trades) = equity_curve(&fills, 1000);
        assert_eq!(trades, 2);
        assert_eq!(pts.len(), 2);
        // +30 then -10 → +20.
        assert!((period - 20.0).abs() < 1e-9, "got {period}");
        assert!((pts[0].v - 30.0).abs() < 1e-9);
    }

    #[test]
    fn curve_empty_without_sells() {
        let fills = vec![fill("BTC-USD", "buy", 1.0, 100.0, 1100)];
        let (pts, period, trades) = equity_curve(&fills, 1000);
        assert!(pts.is_empty() && period == 0.0 && trades == 0);
    }

    #[test]
    fn svg_renders_line_and_labels() {
        let pts = vec![
            EquityPoint { t: 1100, v: 10.0, trades: 1 },
            EquityPoint { t: 1200, v: -5.0, trades: 2 },
        ];
        let svg = equity_svg(&pts, 1000, 1300, false);
        assert!(svg.contains("<svg") && svg.contains("<path"));
        assert!(svg.contains("+10.00") && svg.contains("-5.00"));
    }

    #[test]
    fn svg_empty_period_is_flat_zero() {
        let svg = equity_svg(&[], 1000, 2000, false);
        assert!(svg.contains("<svg") && svg.contains("<path"));
        assert!(!svg.contains("<circle"), "no end dot without trades");
    }

    // --- Database-backed proof (same gate as risk.rs) --------------------
    // Skipped unless WARDEN_TEST_DATABASE_URL points at a scratch Postgres.

    async fn proof_pool() -> Option<sqlx::PgPool> {
        match std::env::var("WARDEN_TEST_DATABASE_URL") {
            Ok(url) => Some(sqlx::PgPool::connect(&url).await.expect("proof DB")),
            Err(_) => {
                eprintln!("skipped: WARDEN_TEST_DATABASE_URL unset");
                None
            }
        }
    }

    #[tokio::test]
    async fn db_curve_matches_inserted_fills() {
        let Some(pool) = proof_pool().await else { return };
        let tenant = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO tenants (id, name) VALUES ($1, 'equity-proof')")
            .bind(tenant)
            .execute(&pool)
            .await
            .expect("insert tenant");
        for (sym, side, qty, price) in [
            ("BTC-USD", "buy", 1.0, 100.0),
            ("BTC-USD", "sell", 1.0, 150.0),
            ("ETH-USD", "buy", 2.0, 50.0),
            ("ETH-USD", "sell", 2.0, 40.0),
        ] {
            sqlx::query(
                "INSERT INTO paper_orders (tenant_id, exchange, symbol, side, qty, price, status)
                 VALUES ($1, 'coinbase', $2, $3, $4, $5, 'filled')",
            )
            .bind(tenant)
            .bind(sym)
            .bind(side)
            .bind(qty)
            .bind(price)
            .execute(&pool)
            .await
            .expect("insert fill");
        }
        let fills: Vec<(String, String, f64, f64, i64)> = sqlx::query_as(
            "SELECT symbol, side, qty, price, extract(epoch FROM created_at)::BIGINT AS ts
             FROM paper_orders WHERE tenant_id = $1 ORDER BY created_at ASC",
        )
        .bind(tenant)
        .fetch_all(&pool)
        .await
        .expect("load fills");
        assert_eq!(fills.len(), 4);
        let (pts, period, trades) = equity_curve(&fills, 0);
        assert_eq!(trades, 2);
        assert!((period - 30.0).abs() < 1e-9, "got {period}");
        assert_eq!(pts.len(), 2);
        assert!(equity_svg(&pts, 0, chrono::Utc::now().timestamp(), false).contains("+50.00"));
    }
}
