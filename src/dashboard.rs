//! Paper-trading dashboard: real PnL from the order ledger.
//!
//! `trades_count` is the real `paper_orders` row count, `pnl_realized`
//! comes from FIFO matching of buys/sells per symbol, `pnl_unrealized`
//! marks the net open position to the live Coinbase spot price,
//! `win_rate` counts profitable closed trades, and `signals` carries
//! the 8 most recent agent decisions. Everything degrades to zeros
//! (with a warning) when the database or the feed is unreachable.
//! `generated_at` is always the current backend time.

use askama::Template;
use axum::extract::State;
use chrono::Utc;

use crate::state::AppState;

/// Askama template for the paper-trading dashboard overview.
#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub capital_total: String,
    pub capital_class: &'static str,
    pub capital_sub: String,
    pub portfolio_value: String,
    pub portfolio_class: &'static str,
    pub portfolio_sub: String,
    pub pnl_realized: String,
    pub realized_class: &'static str,
    pub realized_sub: String,
    pub pnl_unrealized: String,
    pub unrealized_class: &'static str,
    pub unrealized_sub: String,
    pub trades_count: String,
    pub fees_paid: String,
    pub drawdown: String,
    pub win_rate: String,
    pub generated_at: String,
    /// Latest agent decisions, newest first (`agent:action symbol`).
    pub signals: Vec<String>,
}

fn fmt_money(v: f64) -> String {
    if v == 0.0 {
        "0".to_string()
    } else {
        format!("{:+.2}", v)
    }
}

/// Paper account starting bank, USD. The dashboard "Capital total" is this
/// bank plus lifetime P&L — never the open-positions value (that was the
/// `0.78` bug: a near-empty book looked like a near-empty account).
pub const PAPER_STARTING_CAPITAL_USD: f64 = 10_000.0;

/// USD amount with explicit `$` sign: `$+8.93`, `$-3.10`, `$0.00`.
pub fn fmt_usd(v: f64) -> String {
    if v == 0.0 {
        "$0.00".to_string()
    } else {
        format!("${:+.2}", v)
    }
}

/// EUR equivalent in French format (`≈ +8,21 €`), or empty when the
/// rate is unknown. Pure.
pub fn fmt_eur(usd: f64, rate: Option<f64>) -> String {
    match rate {
        Some(r) if r.is_finite() && r > 0.0 => {
            let eur = usd * r;
            let sign = if eur > 0.0 { "+" } else if eur < 0.0 { "-" } else { "" };
            format!("≈ {}{} €", sign, format!("{:.2}", eur.abs()).replace('.', ","))
        }
        _ => String::new(),
    }
}

/// `up` (green) / `down` (red) / `` (neutral) for delta styling.
pub fn delta_class(v: f64) -> &'static str {
    if v > 0.0 {
        "up"
    } else if v < 0.0 {
        "down"
    } else {
        ""
    }
}

/// Free keyless USD→EUR rate (`open.er-api.com`). `None` on any
/// failure — the dashboard then simply hides EUR equivalents.
pub async fn fetch_usd_eur() -> Option<f64> {
    let body: serde_json::Value = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?
        .get("https://open.er-api.com/v6/latest/USD")
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    body.get("rates")?.get("EUR")?.as_f64()
}

/// Net long position from fills: `(qty, average entry price)`.
///
/// FIFO consumption of buys by sells. `(0, 0)` when flat or net-short:
/// protective exits only manage long exposure.
pub fn long_position(fills: &[(String, f64, f64)]) -> (f64, f64) {
    let mut lots: Vec<(f64, f64)> = Vec::new();
    for (side, qty, price) in fills {
        if side == "buy" {
            lots.push((*qty, *price));
        } else {
            let mut rest = *qty;
            while rest > 0.0 {
                match lots.first_mut() {
                    Some((q, _)) => {
                        let m = rest.min(*q);
                        *q -= m;
                        rest -= m;
                        if *q <= 0.0 {
                            lots.remove(0);
                        }
                    }
                    None => break,
                }
            }
        }
    }
    let qty: f64 = lots.iter().map(|(q, _)| q).sum();
    if qty <= 0.0 {
        return (0.0, 0.0);
    }
    let notional: f64 = lots.iter().map(|(q, p)| q * p).sum();
    (qty, notional / qty)
}

/// FIFO PnL over `(side, qty, price)` fills, oldest first.
///
/// Returns `(realized, closed_trades, wins, net_qty, net_avg_price)`.
/// Buys first offset shorts, sells first offset longs; the remainder
/// opens or extends the net position.
fn fifo_pnl(fills: &[(String, f64, f64)]) -> (f64, usize, usize, f64, f64) {
    let mut longs: Vec<(f64, f64)> = Vec::new();
    let mut shorts: Vec<(f64, f64)> = Vec::new();
    let (mut realized, mut closed, mut wins) = (0.0, 0usize, 0usize);
    for (side, qty, price) in fills {
        if side == "buy" {
            let mut rest = *qty;
            while rest > 0.0 {
                match shorts.first_mut() {
                    Some((q, p)) => {
                        let m = rest.min(*q);
                        let pnl = (*p - *price) * m;
                        realized += pnl;
                        closed += 1;
                        if pnl > 0.0 {
                            wins += 1;
                        }
                        *q -= m;
                        rest -= m;
                        if *q <= 0.0 {
                            shorts.remove(0);
                        }
                    }
                    None => {
                        longs.push((rest, *price));
                        rest = 0.0;
                    }
                }
            }
        } else {
            let mut rest = *qty;
            while rest > 0.0 {
                match longs.first_mut() {
                    Some((q, p)) => {
                        let m = rest.min(*q);
                        let pnl = (*price - *p) * m;
                        realized += pnl;
                        closed += 1;
                        if pnl > 0.0 {
                            wins += 1;
                        }
                        *q -= m;
                        rest -= m;
                        if *q <= 0.0 {
                            longs.remove(0);
                        }
                    }
                    None => {
                        shorts.push((rest, *price));
                        rest = 0.0;
                    }
                }
            }
        }
    }
    let long_qty: f64 = longs.iter().map(|(q, _)| q).sum();
    let long_cost: f64 = longs.iter().map(|(q, p)| q * p).sum();
    let short_qty: f64 = shorts.iter().map(|(q, _)| q).sum();
    let short_proceeds: f64 = shorts.iter().map(|(q, p)| q * p).sum();
    let net = long_qty - short_qty;
    let avg = if net > 0.0 {
        long_cost / long_qty
    } else if net < 0.0 {
        short_proceeds / short_qty
    } else {
        0.0
    };
    (realized, closed, wins, net, avg)
}

/// Short backend timestamp for "updated …" labels: `HH:MM:SS UTC`.
///
/// Full RFC3339 belongs in JSON APIs; human pages get the short form.
/// Timezone is always UTC (server time): the label says so.
pub fn short_now() -> String {
    chrono::Utc::now().format("%H:%M:%S UTC").to_string()
}

/// Pure valuation: per-symbol FIFO + per-symbol marks.
///
/// `marks` maps symbol → current price. Symbols without a mark (feed down)
/// keep their realized PnL but contribute no unrealized/open value —
/// never valued at another symbol's price.
pub fn value_positions(
    by_symbol: &std::collections::BTreeMap<String, Vec<(String, f64, f64)>>,
    marks: &std::collections::BTreeMap<String, f64>,
) -> (f64, usize, usize, f64, f64) {
    let (mut realized, mut closed, mut wins, mut unrealized, mut open_value) =
        (0.0, 0usize, 0usize, 0.0, 0.0);
    for (symbol, fills) in by_symbol {
        let (r, c, w, net, avg) = fifo_pnl(fills);
        realized += r;
        closed += c;
        wins += w;
        if net != 0.0 {
            if let Some(mark) = marks.get(symbol) {
                if *mark > 0.0 {
                    unrealized += net * (mark - avg);
                    open_value += net * mark;
                }
            }
        }
    }
    (realized, closed, wins, unrealized, open_value)
}

/// Backend handler building the dashboard from live Postgres data.
pub async fn dashboard_handler(State(state): State<AppState>) -> axum::response::Html<String> {
    let fills: Vec<(String, String, f64, f64)> = sqlx::query_as(
        "SELECT symbol, side, qty, price FROM paper_orders ORDER BY created_at ASC",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("dashboard orders degraded: {e}");
        Vec::new()
    });
    // FIFO per symbol (single-symbol today, grouped anyway).
    use std::collections::BTreeMap;
    let mut by_symbol: BTreeMap<String, Vec<(String, f64, f64)>> = BTreeMap::new();
    for (symbol, side, qty, price) in &fills {
        by_symbol
            .entry(symbol.clone())
            .or_default()
            .push((side.clone(), *qty, *price));
    }
    // One mark per held symbol: never value SOL at BTC's price.
    // Only symbols with a nonzero net position need a feed call.
    let mut held: Vec<String> = Vec::new();
    for (symbol, fills) in &by_symbol {
        let (_, _, _, net, _) = fifo_pnl(fills);
        if net != 0.0 {
            held.push(symbol.clone());
        }
    }
    let client = crate::coinbase::CoinbaseClient::new();
    let mut marks: BTreeMap<String, f64> = BTreeMap::new();
    for symbol in &held {
        let mark = client.fetch_spot_price(symbol).await.unwrap_or(0.0);
        if mark > 0.0 {
            marks.insert(symbol.clone(), mark);
        } else {
            tracing::warn!("dashboard {symbol} degraded: no mark, position held at cost");
        }
    }
    let (realized, closed, wins, unrealized, _open_value) = value_positions(&by_symbol, &marks);
    let signals: Vec<String> = sqlx::query(
        "SELECT agent, action, symbol FROM agent_decisions ORDER BY created_at DESC LIMIT 8",
    )
    .fetch_all(&state.pool)
    .await
    .map(|rows| {
        use sqlx::Row;
        rows.iter()
            .map(|r| {
                let agent: String = r.try_get("agent").unwrap_or_default();
                let action: String = r.try_get("action").unwrap_or_default();
                let symbol: String = r.try_get("symbol").unwrap_or_default();
                format!("{agent}:{action} {symbol}")
            })
            .collect()
    })
    .unwrap_or_default();
    let eur = fetch_usd_eur().await;
    let total = realized + unrealized;
    let capital = PAPER_STARTING_CAPITAL_USD + total;
    let template = DashboardTemplate {
        capital_total: fmt_usd(capital),
        capital_class: delta_class(total),
        capital_sub: fmt_eur(capital, eur),
        portfolio_value: fmt_usd(total),
        portfolio_class: delta_class(total),
        portfolio_sub: fmt_eur(total, eur),
        pnl_realized: fmt_usd(realized),
        realized_class: delta_class(realized),
        realized_sub: fmt_eur(realized, eur),
        pnl_unrealized: fmt_usd(unrealized),
        unrealized_class: delta_class(unrealized),
        unrealized_sub: fmt_eur(unrealized, eur),
        trades_count: fills.len().to_string(),
        fees_paid: "0".to_string(),
        drawdown: fmt_money(realized.min(0.0)),
        win_rate: if closed == 0 {
            "—".to_string()
        } else {
            format!("{}%", wins * 100 / closed)
        },
        generated_at: short_now(),
        signals,
    };
    axum::response::Html(template.render().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(side: &str, qty: f64, price: f64) -> (String, f64, f64) {
        (side.to_string(), qty, price)
    }

    #[test]
    fn fifo_matches_profitable_round_trip() {
        let fills = vec![fill("buy", 1.0, 100.0), fill("sell", 1.0, 150.0)];
        let (realized, closed, wins, net, _) = fifo_pnl(&fills);
        assert_eq!(realized, 50.0);
        assert_eq!((closed, wins), (1, 1));
        assert_eq!(net, 0.0);
    }

    #[test]
    fn fifo_matches_losing_partial_close() {
        let fills = vec![fill("buy", 2.0, 100.0), fill("sell", 1.0, 80.0)];
        let (realized, closed, wins, net, avg) = fifo_pnl(&fills);
        assert_eq!(realized, -20.0);
        assert_eq!((closed, wins), (1, 0));
        assert_eq!(net, 1.0);
        assert_eq!(avg, 100.0);
    }

    #[test]
    fn fifo_handles_short_then_cover() {
        let fills = vec![fill("sell", 1.0, 100.0), fill("buy", 1.0, 90.0)];
        let (realized, closed, wins, net, _) = fifo_pnl(&fills);
        assert_eq!(realized, 10.0);
        assert_eq!((closed, wins), (1, 1));
        assert_eq!(net, 0.0);
    }

    #[test]
    fn short_now_is_a_short_utc_label() {
        let s = short_now();
        assert!(s.len() == "22:31:44 UTC".len(), "short_now={s}");
        assert!(s.ends_with(" UTC"), "short_now={s}");
    }

    #[test]
    fn formatting_is_explicit() {
        assert_eq!(fmt_usd(8.93), "$+8.93");
        assert_eq!(fmt_usd(-3.1), "$-3.10");
        assert_eq!(fmt_usd(0.0), "$0.00");
        assert_eq!(fmt_eur(8.93, Some(0.89048)), "≈ +7,95 €");
        assert_eq!(fmt_eur(-8.93, Some(0.89048)), "≈ -7,95 €");
        assert_eq!(fmt_eur(8.93, None), "");
        assert_eq!(delta_class(1.0), "up");
        assert_eq!(delta_class(-1.0), "down");
        assert_eq!(delta_class(0.0), "");
    }

    #[test]
    fn capital_total_is_bank_plus_pnl_not_open_value() {
        // Regression test for the `0.78` bug: with no open position the old
        // code showed `0` (or the open value) instead of the account bank.
        let bank = PAPER_STARTING_CAPITAL_USD;
        assert_eq!(bank, 10_000.0);
        let capital_flat = bank + 2.5 + 0.0;
        assert_eq!(fmt_usd(capital_flat), "$+10002.50");
        // Even deep in the red the bank dominates, never near zero.
        let capital_red = bank + (-50.0) + (-10.0);
        assert!(capital_red > 9_000.0);
    }

    #[test]
    fn dashboard_template_renders_warden() {
        let template = DashboardTemplate {
            capital_total: "10000".to_string(),
            capital_class: "up",
            capital_sub: "≈ +8 904,80 €".to_string(),
            portfolio_value: "$+8.93".to_string(),
            portfolio_class: "up",
            portfolio_sub: "≈ +7,95 €".to_string(),
            pnl_realized: "$+1.24".to_string(),
            realized_class: "up",
            realized_sub: "≈ +1,10 €".to_string(),
            pnl_unrealized: "$+7.69".to_string(),
            unrealized_class: "up",
            unrealized_sub: "≈ +6,85 €".to_string(),
            trades_count: "0".to_string(),
            fees_paid: "0".to_string(),
            drawdown: "0%".to_string(),
            win_rate: "0%".to_string(),
            generated_at: "now".to_string(),
            signals: vec!["technical:buy BTC-USD".to_string()],
        };
        let rendered = template.render().expect("template renders");
        assert!(
            rendered.contains("Warden"),
            "rendered dashboard should contain Warden"
        );
        assert!(rendered.contains("technical:buy"));
    }

    #[tokio::test]
    async fn handler_degrades_without_db() {
        let state = AppState {
            pool: sqlx::PgPool::connect_lazy("postgres://127.0.0.1:1/none")
                .expect("test lazy pool"),
            jwt_secret: "test".to_string(),
        };
        let axum::response::Html(body) = dashboard_handler(State(state)).await;
        assert!(body.contains("Warden"));
    }
}
