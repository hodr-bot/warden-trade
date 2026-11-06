//! Real trading strategies: pure signal functions over price history.
//!
//! Three complementary, deterministic strategies feed the agent pipeline
//! ([`crate::orchestrator::run_cycle_with_prices`]):
//! - [`sma_signal`]: trend following, SMA(3) vs SMA(8) crossover.
//! - [`mean_reversion_signal`]: z-score fade on a 20-price window.
//! - [`momentum_signal`]: 10-period rate-of-change breakout.
//!
//! [`aggregate`] merges the three votes by majority; ties and empty
//! inputs degrade to `Hold`. Pure logic: no network, no secrets, no I/O.

/// Trade intent voted by a strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Buy,
    Sell,
    Hold,
}

impl Signal {
    /// Stable lowercase action string used in [`crate::agents::AgentDecision`].
    pub fn action(self) -> &'static str {
        match self {
            Signal::Buy => "buy",
            Signal::Sell => "sell",
            Signal::Hold => "hold",
        }
    }
}

/// One strategy's vote with its confidence and human-readable reason.
#[derive(Debug, Clone)]
pub struct StrategySignal {
    pub strategy: &'static str,
    pub signal: Signal,
    pub confidence: f64,
    pub reason: String,
}

fn sma_of(prices: &[f64], end: usize, window: usize) -> f64 {
    prices[end + 1 - window..=end].iter().sum::<f64>() / window as f64
}

/// Trend signal: fast SMA(3) crossing slow SMA(8).
///
/// `Buy` when fast crosses above slow on the last price, `Sell` on the
/// downward cross, `Hold` otherwise (including trending-without-cross
/// and series shorter than 9 prices). Confidence is fixed at 0.7: a
/// cross is a discrete event, not a calibrated probability.
pub fn sma_signal(closes: &[f64]) -> StrategySignal {
    const FAST: usize = 3;
    const SLOW: usize = 8;
    let hold = |reason: &str| StrategySignal {
        strategy: "sma-3-8",
        signal: Signal::Hold,
        confidence: 0.0,
        reason: reason.to_string(),
    };
    if closes.len() < SLOW + 1 {
        return hold("slow window never fills");
    }
    let last = closes.len() - 1;
    let fast_now = sma_of(closes, last, FAST);
    let slow_now = sma_of(closes, last, SLOW);
    let fast_prev = sma_of(closes, last - 1, FAST);
    let slow_prev = sma_of(closes, last - 1, SLOW);
    if fast_prev <= slow_prev && fast_now > slow_now {
        StrategySignal {
            strategy: "sma-3-8",
            signal: Signal::Buy,
            confidence: 0.7,
            reason: format!("fast {fast_now:.2} crossed above slow {slow_now:.2}"),
        }
    } else if fast_prev >= slow_prev && fast_now < slow_now {
        StrategySignal {
            strategy: "sma-3-8",
            signal: Signal::Sell,
            confidence: 0.7,
            reason: format!("fast {fast_now:.2} crossed below slow {slow_now:.2}"),
        }
    } else {
        hold(&format!("no cross (fast {fast_now:.2} vs slow {slow_now:.2})"))
    }
}

/// Mean-reversion signal: fade a stretched z-score on 20 prices.
///
/// `Buy` when the last close sits more than 1.5 std below the mean
/// (oversold), `Sell` above +1.5 (overbought), `Hold` inside the band,
/// on zero variance, or with fewer than 20 prices.
pub fn mean_reversion_signal(closes: &[f64]) -> StrategySignal {
    const WINDOW: usize = 20;
    const ENTRY_Z: f64 = 1.5;
    let hold = |reason: &str| StrategySignal {
        strategy: "mean-reversion-20",
        signal: Signal::Hold,
        confidence: 0.0,
        reason: reason.to_string(),
    };
    if closes.len() < WINDOW {
        return hold("window never fills");
    }
    let window = &closes[closes.len() - WINDOW..];
    let mean = window.iter().sum::<f64>() / WINDOW as f64;
    let var = window.iter().map(|p| (p - mean).powi(2)).sum::<f64>() / WINDOW as f64;
    let std = var.sqrt();
    if std <= 0.0 || !std.is_finite() {
        return hold("zero variance");
    }
    let last = closes[closes.len() - 1];
    let z = (last - mean) / std;
    if z <= -ENTRY_Z {
        StrategySignal {
            strategy: "mean-reversion-20",
            signal: Signal::Buy,
            confidence: 0.6,
            reason: format!("oversold z={z:.2}"),
        }
    } else if z >= ENTRY_Z {
        StrategySignal {
            strategy: "mean-reversion-20",
            signal: Signal::Sell,
            confidence: 0.6,
            reason: format!("overbought z={z:.2}"),
        }
    } else {
        hold(&format!("inside band z={z:.2}"))
    }
}

/// Momentum signal: 10-period rate of change.
///
/// `Buy` above +2%, `Sell` below -2%, `Hold` inside or with fewer
/// than 11 prices. Confidence is fixed at 0.55: momentum alone is
/// the noisiest of the three votes.
pub fn momentum_signal(closes: &[f64]) -> StrategySignal {
    const LOOKBACK: usize = 10;
    const ENTRY_PCT: f64 = 0.02;
    let hold = |reason: &str| StrategySignal {
        strategy: "momentum-10",
        signal: Signal::Hold,
        confidence: 0.0,
        reason: reason.to_string(),
    };
    if closes.len() < LOOKBACK + 1 {
        return hold("lookback never fills");
    }
    let last = closes[closes.len() - 1];
    let base = closes[closes.len() - 1 - LOOKBACK];
    if base <= 0.0 || !base.is_finite() || !last.is_finite() {
        return hold("non-positive base");
    }
    let roc = (last - base) / base;
    if roc >= ENTRY_PCT {
        StrategySignal {
            strategy: "momentum-10",
            signal: Signal::Buy,
            confidence: 0.55,
            reason: format!("roc10={:.2}%", roc * 100.0),
        }
    } else if roc <= -ENTRY_PCT {
        StrategySignal {
            strategy: "momentum-10",
            signal: Signal::Sell,
            confidence: 0.55,
            reason: format!("roc10={:.2}%", roc * 100.0),
        }
    } else {
        hold(&format!("flat roc10={:.2}%", roc * 100.0))
    }
}

/// Merge strategy votes by majority.
///
/// Most `Buy` wins `Buy`, most `Sell` wins `Sell`; ties, empty input
/// and all-`Hold` degrade to `Hold`. Deterministic and total.
pub fn aggregate(signals: &[StrategySignal]) -> Signal {
    let (mut buys, mut sells) = (0usize, 0usize);
    for s in signals {
        match s.signal {
            Signal::Buy => buys += 1,
            Signal::Sell => sells += 1,
            Signal::Hold => {}
        }
    }
    if buys > sells && buys > 0 {
        Signal::Buy
    } else if sells > buys && sells > 0 {
        Signal::Sell
    } else {
        Signal::Hold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sma_fires_buy_on_upward_cross() {
        // Flat base then a final jump: fast crosses above slow exactly
        // on the last bar.
        let mut closes = vec![100.0; 12];
        closes.push(110.0);
        let s = sma_signal(&closes);
        assert_eq!(s.signal, Signal::Buy);
        assert_eq!(s.signal.action(), "buy");
    }

    #[test]
    fn sma_fires_sell_on_downward_cross() {
        let mut closes = vec![100.0; 12];
        closes.push(90.0);
        let s = sma_signal(&closes);
        assert_eq!(s.signal, Signal::Sell);
    }

    #[test]
    fn sma_holds_without_cross_and_when_short() {
        let flat = vec![100.0; 15];
        assert_eq!(sma_signal(&flat).signal, Signal::Hold);
        assert_eq!(sma_signal(&[100.0, 101.0]).signal, Signal::Hold);
        assert_eq!(sma_signal(&[]).signal, Signal::Hold);
    }

    #[test]
    fn mean_reversion_buys_oversold_sells_overbought() {
        let mut base = vec![100.0; 19];
        base.push(90.0); // sharp dip: oversold
        assert_eq!(mean_reversion_signal(&base).signal, Signal::Buy);
        let mut top = vec![100.0; 19];
        top.push(110.0); // sharp spike: overbought
        assert_eq!(mean_reversion_signal(&top).signal, Signal::Sell);
        assert_eq!(mean_reversion_signal(&vec![100.0; 20]).signal, Signal::Hold);
        assert_eq!(mean_reversion_signal(&[100.0; 5]).signal, Signal::Hold);
    }

    #[test]
    fn momentum_follows_rate_of_change() {
        let up: Vec<f64> = (0..12).map(|i| 100.0 + i as f64).collect(); // +11%
        assert_eq!(momentum_signal(&up).signal, Signal::Buy);
        let down: Vec<f64> = (0..12).map(|i| 100.0 - i as f64).collect(); // -11%
        assert_eq!(momentum_signal(&down).signal, Signal::Sell);
        assert_eq!(momentum_signal(&vec![100.0; 12]).signal, Signal::Hold);
        assert_eq!(momentum_signal(&[100.0; 5]).signal, Signal::Hold);
    }

    #[test]
    fn aggregate_majority_wins_ties_hold() {
        let buy = StrategySignal {
            strategy: "a",
            signal: Signal::Buy,
            confidence: 1.0,
            reason: "t".to_string(),
        };
        let sell = StrategySignal {
            strategy: "b",
            signal: Signal::Sell,
            confidence: 1.0,
            reason: "t".to_string(),
        };
        let hold = StrategySignal {
            strategy: "c",
            signal: Signal::Hold,
            confidence: 0.0,
            reason: "t".to_string(),
        };
        assert_eq!(aggregate(&[buy.clone(), buy.clone(), sell.clone()]), Signal::Buy);
        assert_eq!(aggregate(&[sell.clone(), sell.clone(), buy.clone()]), Signal::Sell);
        assert_eq!(aggregate(&[buy.clone(), sell.clone()]), Signal::Hold);
        assert_eq!(aggregate(&[hold.clone(), hold.clone()]), Signal::Hold);
        assert_eq!(aggregate(&[]), Signal::Hold);
    }

    #[test]
    fn signal_action_strings() {
        assert_eq!(Signal::Buy.action(), "buy");
        assert_eq!(Signal::Sell.action(), "sell");
        assert_eq!(Signal::Hold.action(), "hold");
    }
}
