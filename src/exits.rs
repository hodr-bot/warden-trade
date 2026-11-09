//! Protective exits, inspired by freqtrade's stoploss / minimal-ROI model.
//!
//! Before each signal cycle, the scheduler asks [`check_exit`] whether the
//! open long position on a symbol must be closed:
//! - **take-profit**: price >= entry average x (1 + `TAKE_PROFIT_PCT`);
//! - **stoploss**: price <= entry average x (1 - `STOP_LOSS_PCT`), or —
//!   when `TRAILING_STOP` is on — price <= highest price seen since entry
//!   x (1 - `STOP_LOSS_PCT`);
//! - **cooldown**: after an exit, no re-entry for `EXIT_COOLDOWN_TICKS`
//!   ticks (freqtrade's "pair lock" idea), so a stop is not instantly
//!   re-bought by the same signal.
//!
//! Peaks and cooldowns live in process memory ([`EXIT_STATE`]); a restart
//! re-seeds the peak at `max(entry average, current price)`. Documented and
//! acceptable in paper mode. Agents cannot change these parameters: they
//! come from env only (`STOP_LOSS_PCT` default 0.02, `TAKE_PROFIT_PCT`
//! default 0.05, `TRAILING_STOP` default true, `EXIT_COOLDOWN_TICKS`
//! default 3; a 0 percentage disables that exit).

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Net long position: quantity held and average entry price.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Position {
    pub qty: f64,
    pub avg: f64,
}

/// Outcome of [`check_exit`].
#[derive(Debug, Clone, PartialEq)]
pub enum ExitCheck {
    /// Flat, exit disabled, or cooldown tick consumed without exit.
    Hold,
    /// Cooldown active after a recent exit: skip the signal cycle.
    Cooldown { ticks_left: u32 },
    /// Close the whole position now.
    Exit {
        qty: f64,
        reason: String,
        kind: ExitKind,
    },
}

/// Which protection fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitKind {
    StopLoss,
    TakeProfit,
}

#[derive(Debug, Default)]
struct SymbolState {
    peak: f64,
    cooldown_left: u32,
}

static EXIT_STATE: OnceLock<Mutex<HashMap<String, SymbolState>>> = OnceLock::new();

fn state() -> &'static Mutex<HashMap<String, SymbolState>> {
    EXIT_STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Reset in-memory peaks/cooldowns. Tests only.
#[cfg(test)]
pub fn reset_exit_state() {
    state().lock().expect("exit state").clear();
}

fn env_pct(name: &str, def: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(def)
        .max(0.0)
}

fn env_u32(name: &str, def: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(def)
}

fn env_bool(name: &str, def: bool) -> bool {
    std::env::var(name)
        .ok()
        .map(|v| {
            let v = v.to_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        })
        .unwrap_or(def)
}

/// Decide whether `price` must close `position` on `symbol`.
///
/// Reads env on every call so tests and operators can retune without a
/// restart. Consumes one cooldown tick when cooling down; arms the cooldown
/// when an exit fires.
pub fn check_exit(symbol: &str, price: f64, position: Option<Position>) -> ExitCheck {
    let stop_pct = env_pct("STOP_LOSS_PCT", 0.02);
    let tp_pct = env_pct("TAKE_PROFIT_PCT", 0.05);
    let trailing = env_bool("TRAILING_STOP", true);
    let cooldown_ticks = env_u32("EXIT_COOLDOWN_TICKS", 3);

    let mut guard = state().lock().expect("exit state");
    let entry = guard.entry(symbol.to_string()).or_default();

    // Cooldown consumes a tick even while flat: a restart keeps no memory
    // of it, which only delays re-entry, never forces a trade.
    if entry.cooldown_left > 0 {
        entry.cooldown_left -= 1;
        let left = entry.cooldown_left;
        if position.map(|p| p.qty).unwrap_or(0.0) <= 0.0 {
            entry.peak = 0.0;
        }
        return if left > 0 {
            ExitCheck::Cooldown { ticks_left: left }
        } else {
            ExitCheck::Hold
        };
    }

    let Some(pos) = position else {
        entry.peak = 0.0;
        return ExitCheck::Hold;
    };
    if pos.qty <= 0.0 || pos.avg <= 0.0 || price <= 0.0 {
        return ExitCheck::Hold;
    }

    // Take-profit wins over the stop when both bands touch (gap move).
    if tp_pct > 0.0 && price >= pos.avg * (1.0 + tp_pct) {
        entry.peak = 0.0;
        entry.cooldown_left = cooldown_ticks;
        return ExitCheck::Exit {
            qty: pos.qty,
            reason: format!(
                "take-profit: {symbol} {price:.2} >= entry {avg:.2} x {mult:.3}",
                avg = pos.avg,
                mult = 1.0 + tp_pct,
            ),
            kind: ExitKind::TakeProfit,
        };
    }

    if stop_pct > 0.0 {
        if entry.peak < pos.avg.max(price) {
            entry.peak = pos.avg.max(price);
        }
        let reference = if trailing { entry.peak } else { pos.avg };
        if price <= reference * (1.0 - stop_pct) {
            entry.peak = 0.0;
            entry.cooldown_left = cooldown_ticks;
            let mode = if trailing { "trailing" } else { "fixed" };
            return ExitCheck::Exit {
                qty: pos.qty,
                reason: format!(
                    "stop-loss ({mode}): {symbol} {price:.2} <= ref {reference:.2} x {mult:.3}",
                    mult = 1.0 - stop_pct,
                ),
                kind: ExitKind::StopLoss,
            };
        }
    }

    ExitCheck::Hold
}

/// Mark a protective exit so the next ticks cool down instead of
/// re-entering on the same signal. Used after a sell is placed.
pub fn note_exit(symbol: &str) {
    let cooldown_ticks = env_u32("EXIT_COOLDOWN_TICKS", 3);
    let mut guard = state().lock().expect("exit state");
    let entry = guard.entry(symbol.to_string()).or_default();
    entry.peak = 0.0;
    entry.cooldown_left = cooldown_ticks;
}

/// Current cooldown for `symbol`, 0 when free to trade.
#[cfg(test)]
fn cooldown_left(symbol: &str) -> u32 {
    state()
        .lock()
        .expect("exit state")
        .get(symbol)
        .map(|s| s.cooldown_left)
        .unwrap_or(0)
}

/// Spot-sized sell capped at the held long: never sell what we don't own.
///
/// Returns `0` when flat (caller skips: no short selling on spot paper).
/// Otherwise `min(sized, held)`: a profitable signal takes gains without
/// inventing exposure.
pub fn cap_sell_qty(sized: f64, held: f64) -> f64 {
    if held <= 0.0 || sized <= 0.0 {
        0.0
    } else {
        sized.min(held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    /// Serializes env-mutating tests inside this module (no external crate).
    static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.get_or_init(|| Mutex::new(())).lock().expect("test lock")
    }

    fn pos(qty: f64, avg: f64) -> Option<Position> {
        Some(Position { qty, avg })
    }

    #[test]
    fn no_short_selling() {
        let _guard = lock();
        reset_exit_state();
        // Flat: nothing to sell, whatever the signal sizes.
        assert_eq!(cap_sell_qty(1.5, 0.0), 0.0);
        // Net-short legacy (pre-guard fills): still nothing to sell.
        assert_eq!(cap_sell_qty(1.5, -0.4), 0.0);
        // Long: capped at the held amount.
        assert_eq!(cap_sell_qty(1.5, 0.008), 0.008);
        assert_eq!(cap_sell_qty(0.002, 0.008), 0.002);
    }

    #[test]
    fn per_symbol_marks_never_cross() {
        use crate::dashboard::value_positions;
        use std::collections::BTreeMap;
        let _guard = lock();
        // The live -$84k bug: a SOL short valued at BTC's mark.
        let mut by: BTreeMap<String, Vec<(String, f64, f64)>> = BTreeMap::new();
        by.insert(
            "BTC-USD".to_string(),
            vec![("buy".to_string(), 0.01, 75000.0)],
        );
        by.insert(
            "SOL-USD".to_string(),
            vec![("sell".to_string(), 1.109, 90.16)],
        );
        let mut marks = BTreeMap::new();
        marks.insert("BTC-USD".to_string(), 75918.0);
        marks.insert("SOL-USD".to_string(), 90.12);
        let (realized, _, _, unrealized, open) = value_positions(&by, &marks);
        // BTC long: +9.18 unrealized. SOL short: (90.12-90.16)*1.109 ≈ -0.04.
        // Total must stay near zero — never -$84k.
        assert!(unrealized > 9.0 && unrealized < 9.3, "unrealized={unrealized}");
        assert!(open > 600.0 && open < 900.0, "open={open}");
        assert!(realized.abs() < 0.01, "realized={realized}");
        // Missing mark: no hallucinated value, realized preserved.
        let empty: BTreeMap<String, f64> = BTreeMap::new();
        let (r2, _, _, u2, o2) = value_positions(&by, &empty);
        assert_eq!(u2, 0.0);
        assert_eq!(o2, 0.0);
        assert_eq!(r2, realized);
    }

    #[test]
    fn default_stop_is_two_percent() {
        let _guard = lock();
        reset_exit_state();
        // Empty string does not parse: falls back to the compiled default.
        std::env::set_var("STOP_LOSS_PCT", "");
        std::env::set_var("TAKE_PROFIT_PCT", "0.0");
        std::env::set_var("TRAILING_STOP", "false");
        std::env::set_var("EXIT_COOLDOWN_TICKS", "0");
        // Entry 100: -1% holds, past -2% exits.
        assert_eq!(check_exit("BTC-USD", 99.0, pos(1.0, 100.0)), ExitCheck::Hold);
        match check_exit("ETH-USD", 97.9, pos(1.0, 100.0)) {
            ExitCheck::Exit { kind, .. } => assert_eq!(kind, ExitKind::StopLoss),
            other => panic!("expected default stop exit, got {other:?}"),
        }
        std::env::remove_var("STOP_LOSS_PCT");
        std::env::remove_var("TAKE_PROFIT_PCT");
        std::env::remove_var("TRAILING_STOP");
        std::env::remove_var("EXIT_COOLDOWN_TICKS");
    }

    #[test]
    fn flat_position_never_exits() {
        let _guard = lock();
        reset_exit_state();
        std::env::set_var("STOP_LOSS_PCT", "0.10");
        std::env::set_var("TAKE_PROFIT_PCT", "0.05");
        assert_eq!(check_exit("BTC-USD", 50_000.0, None), ExitCheck::Hold);
        assert_eq!(
            check_exit("BTC-USD", 50_000.0, pos(0.0, 60_000.0)),
            ExitCheck::Hold
        );
        std::env::remove_var("STOP_LOSS_PCT");
        std::env::remove_var("TAKE_PROFIT_PCT");
    }

    #[test]
    fn fixed_stop_fires_below_band() {
        let _guard = lock();
        reset_exit_state();
        std::env::set_var("STOP_LOSS_PCT", "0.10");
        std::env::set_var("TAKE_PROFIT_PCT", "0.0");
        std::env::set_var("TRAILING_STOP", "false");
        std::env::set_var("EXIT_COOLDOWN_TICKS", "2");
        // Entry 100: -5% holds, -10% exits.
        assert_eq!(check_exit("BTC-USD", 95.0, pos(1.0, 100.0)), ExitCheck::Hold);
        match check_exit("BTC-USD", 89.0, pos(1.0, 100.0)) {
            ExitCheck::Exit { kind, .. } => assert_eq!(kind, ExitKind::StopLoss),
            other => panic!("expected stop exit, got {other:?}"),
        }
        assert_eq!(cooldown_left("BTC-USD"), 2);
        std::env::remove_var("STOP_LOSS_PCT");
        std::env::remove_var("TAKE_PROFIT_PCT");
        std::env::remove_var("TRAILING_STOP");
        std::env::remove_var("EXIT_COOLDOWN_TICKS");
    }

    #[test]
    fn take_profit_fires_above_band() {
        let _guard = lock();
        reset_exit_state();
        std::env::set_var("STOP_LOSS_PCT", "0.0");
        std::env::set_var("TAKE_PROFIT_PCT", "0.05");
        match check_exit("ETH-USD", 105.01, pos(2.0, 100.0)) {
            ExitCheck::Exit { qty, kind, .. } => {
                assert_eq!(kind, ExitKind::TakeProfit);
                assert!((qty - 2.0).abs() < 1e-9);
            }
            other => panic!("expected take-profit, got {other:?}"),
        }
        std::env::remove_var("STOP_LOSS_PCT");
        std::env::remove_var("TAKE_PROFIT_PCT");
    }

    #[test]
    fn trailing_stop_follows_the_peak() {
        let _guard = lock();
        reset_exit_state();
        std::env::set_var("STOP_LOSS_PCT", "0.10");
        std::env::set_var("TAKE_PROFIT_PCT", "0.0");
        std::env::set_var("TRAILING_STOP", "true");
        std::env::set_var("EXIT_COOLDOWN_TICKS", "1");
        // Entry 100, rally to 120: peak 120, stop now at 108.
        assert_eq!(check_exit("SOL-USD", 110.0, pos(1.0, 100.0)), ExitCheck::Hold);
        assert_eq!(check_exit("SOL-USD", 120.0, pos(1.0, 100.0)), ExitCheck::Hold);
        // Pullback to 109: above 108, holds.
        assert_eq!(check_exit("SOL-USD", 109.0, pos(1.0, 100.0)), ExitCheck::Hold);
        // Pullback to 107: stop fires even though still above entry.
        match check_exit("SOL-USD", 107.0, pos(1.0, 100.0)) {
            ExitCheck::Exit { kind, .. } => assert_eq!(kind, ExitKind::StopLoss),
            other => panic!("expected trailing stop, got {other:?}"),
        }
        std::env::remove_var("STOP_LOSS_PCT");
        std::env::remove_var("TAKE_PROFIT_PCT");
        std::env::remove_var("TRAILING_STOP");
        std::env::remove_var("EXIT_COOLDOWN_TICKS");
    }

    #[test]
    fn cooldown_blocks_reentry_then_releases() {
        let _guard = lock();
        reset_exit_state();
        std::env::set_var("STOP_LOSS_PCT", "0.10");
        std::env::set_var("TAKE_PROFIT_PCT", "0.0");
        std::env::set_var("TRAILING_STOP", "false");
        std::env::set_var("EXIT_COOLDOWN_TICKS", "2");
        match check_exit("BTC-USD", 80.0, pos(1.0, 100.0)) {
            ExitCheck::Exit { .. } => {}
            other => panic!("expected exit, got {other:?}"),
        }
        // Flat now: cooldown ticks down 2 -> 1 -> free.
        match check_exit("BTC-USD", 80.0, None) {
            ExitCheck::Cooldown { ticks_left } => assert_eq!(ticks_left, 1),
            other => panic!("expected cooldown, got {other:?}"),
        }
        assert_eq!(check_exit("BTC-USD", 80.0, None), ExitCheck::Hold);
        std::env::remove_var("STOP_LOSS_PCT");
        std::env::remove_var("TAKE_PROFIT_PCT");
        std::env::remove_var("TRAILING_STOP");
        std::env::remove_var("EXIT_COOLDOWN_TICKS");
    }

    #[test]
    fn disabled_exits_never_fire() {
        let _guard = lock();
        reset_exit_state();
        std::env::set_var("STOP_LOSS_PCT", "0.0");
        std::env::set_var("TAKE_PROFIT_PCT", "0.0");
        assert_eq!(
            check_exit("BTC-USD", 1.0, pos(1.0, 100.0)),
            ExitCheck::Hold
        );
        std::env::remove_var("STOP_LOSS_PCT");
        std::env::remove_var("TAKE_PROFIT_PCT");
    }
}
