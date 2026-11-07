//! External market context from free keyless sources (no secrets).
//!
//! - [`fetch_sentiment`]: crypto news mood from public RSS feeds
//!   (CoinDesk + CoinTelegraph), scored on keyword lists.
//! - [`fetch_fear_greed`]: the Alternative.me Crypto Fear & Greed index
//!   (0 = extreme fear, 100 = extreme greed), a numbered market mood.
//! - [`fetch_funding`]: Binance perpetual funding rate for a base asset
//!   (e.g. `"BTC"` → `BTCUSDT`); crowded longs (high positive funding)
//!   are contrarian-bearish.
//! - [`combine_sentiment`]: merges RSS mood with Fear & Greed into one
//!   `[-1.0, 1.0]` score for the orchestration gate.
//!
//! Every fetch degrades to neutral (`0.0` / `None`) on failure — context
//! only ever soft-vetoes with a logged reason, it never blocks trading
//! by accident. Network lives behind the `fetch_*` functions;
//! [`score_titles`], [`extract_titles`] and [`combine_sentiment`] are
//! pure and unit-tested.

use std::time::Duration;

/// Public RSS endpoints (redirects followed).
const FEEDS: &[&str] = &[
    "https://www.coindesk.com/arc/outboundfeeds/rss/",
    "https://cointelegraph.com/rss",
];

/// Max titles scored per feed.
const MAX_TITLES: usize = 30;

/// Lowercase keywords voting bullish.
const BULLISH: &[&str] = &[
    "surge", "soar", "rally", "bull", "breakout", "all-time high", "record high", "etf inflow",
    "inflows", "adoption", "approval", "approved", "upgrade", "partnership", "gains", "jumps",
    "rises", "rebound", "recovery", "accumulat",
];

/// Lowercase keywords voting bearish.
const BEARISH: &[&str] = &[
    "crash", "plunge", "bear", "hack", "exploit", "lawsuit", "sues", "sued", "ban", "banned",
    "collapse", "outflow", "outflows", "fraud", "scam", "seiz", "sanction", "falls", "drops",
    "tumbles", "warning", "illicit",
];

/// Decode the few entities/CDATA markers RSS titles actually use.
fn clean_title(raw: &str) -> String {
    let t = raw.trim();
    let t = t.strip_prefix("<![CDATA[").unwrap_or(t);
    let t = t.strip_suffix("]]>").unwrap_or(t);
    t.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .trim()
        .to_string()
}

/// Extract item `<title>` texts from an RSS document.
///
/// Handles plain `<title>text</title>` and `<title><![CDATA[text]]></title>`.
/// The channel-level title (first one, outside any `<item>`) is skipped.
/// Pure: no network.
pub fn extract_titles(xml: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    // Split on items so the channel title never counts.
    let items: Vec<&str> = xml.split("<item>").skip(1).collect();
    for item in items {
        if out.len() >= max {
            break;
        }
        let Some(start) = item.find("<title>") else {
            continue;
        };
        let rest = &item[start + "<title>".len()..];
        let Some(end) = rest.find("</title>") else {
            continue;
        };
        let title = clean_title(&rest[..end]);
        if !title.is_empty() {
            out.push(title);
        }
    }
    out
}

/// Score titles in `[-1.0, 1.0]`: `(bullish - bearish) / scored`.
///
/// Each title votes at most once (bullish wins ties). Empty input is neutral.
/// Pure: no network.
pub fn score_titles(titles: &[String]) -> f32 {
    if titles.is_empty() {
        return 0.0;
    }
    let (mut bull, mut bear) = (0i32, 0i32);
    for t in titles {
        let lower = t.to_lowercase();
        let b = BULLISH.iter().any(|k| lower.contains(k));
        let r = BEARISH.iter().any(|k| lower.contains(k));
        match (b, r) {
            (true, false) => bull += 1,
            (false, true) => bear += 1,
            (true, true) => bull += 1, // headline mentions upside: count the bid
            (false, false) => {}
        }
    }
    (bull - bear) as f32 / titles.len() as f32
}

/// Merge RSS mood with Fear & Greed into one `[-1.0, 1.0]` score.
///
/// Fear & Greed maps linearly (`50 → 0.0`, `0 → -1.0`, `100 → +1.0`)
/// and weighs half; without it the RSS score stands alone.
/// Pure: no network.
pub fn combine_sentiment(rss: f32, fear_greed: Option<u8>) -> f32 {
    match fear_greed {
        Some(fg) => {
            let fg_score = (fg.min(100) as f32 - 50.0) / 50.0;
            (rss + fg_score) / 2.0
        }
        None => rss,
    }
}

/// Fetch the Crypto Fear & Greed index value (`0..=100`).
///
/// Free keyless endpoint (`api.alternative.me/fng`). `None` on any
/// failure (network, shape change, unparsable value). Never panics.
pub async fn fetch_fear_greed() -> Option<u8> {
    let body: serde_json::Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?
        .get("https://api.alternative.me/fng/?limit=1")
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    body.get("data")?
        .get(0)?
        .get("value")?
        .as_str()?
        .parse()
        .ok()
}

/// Fetch the Binance perpetual funding rate for `base` (e.g. `"BTC"`).
///
/// Free keyless endpoint (`fapi.binance.com`). Returns the raw rate
/// (e.g. `0.0001` = 0.01 % per 8 h). `None` on any failure. Never panics.
pub async fn fetch_funding(base: &str) -> Option<f64> {
    let symbol = format!("{}USDT", base.to_uppercase());
    let url = format!("https://fapi.binance.com/fapi/v1/fundingRate?symbol={symbol}&limit=1");
    let body: serde_json::Value = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .ok()?
        .get(url)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    body.get(0)?.get("fundingRate")?.as_str()?.parse().ok()
}

/// Fetch both feeds and return the freshest headlines (for the LLM advisor
/// prompt). At most `max` titles, newest feed first. Empty on failure.
/// Never panics, never logs secrets.
pub async fn fetch_titles(max: usize) -> Vec<String> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("warden-trade/rss")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut out = Vec::new();
    for url in FEEDS {
        let Ok(resp) = http.get(*url).send().await else {
            continue;
        };
        let Ok(body) = resp.text().await else {
            continue;
        };
        out.extend(extract_titles(&body, MAX_TITLES));
        if out.len() >= max {
            break;
        }
    }
    out.truncate(max);
    out
}

/// Fetch both feeds and return the mean sentiment in `[-1.0, 1.0]`.
///
/// Follows redirects, 15 s timeout per feed. A dead feed contributes
/// nothing; all dead → `0.0` neutral. Never panics, never logs secrets.
pub async fn fetch_sentiment() -> f32 {
    fetch_sentiment_from(FEEDS).await
}

/// Same as [`fetch_sentiment`] over explicit feed URLs (tests use local stubs).
async fn fetch_sentiment_from(urls: &[&str]) -> f32 {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("warden-trade/rss")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut scores = Vec::new();
    for url in urls {
        let Ok(resp) = http.get(*url).send().await else {
            continue;
        };
        let Ok(body) = resp.text().await else {
            continue;
        };
        let titles = extract_titles(&body, MAX_TITLES);
        if titles.is_empty() {
            continue;
        }
        scores.push(score_titles(&titles));
    }
    if scores.is_empty() {
        0.0
    } else {
        scores.iter().sum::<f32>() / scores.len() as f32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<?xml version="1.0"?>
<rss><channel><title>Feed News</title>
<item><title>Bitcoin surge to record high on ETF inflows</title></item>
<item><title>Exchange hack drains funds, fraud probe opens</title></item>
<item><title><![CDATA[Solana rebound as upgrade approved &amp; partnership grows]]></title></item>
<item><title>Market outlook: analysts weigh weekend flows</title></item>
</channel></rss>"#;

    #[test]
    fn extract_skips_channel_title_and_handles_cdata() {
        let titles = extract_titles(FIXTURE, 30);
        assert_eq!(titles.len(), 4);
        assert!(titles[0].contains("Bitcoin surge"));
        assert_eq!(
            titles[2],
            "Solana rebound as upgrade approved & partnership grows"
        );
    }

    #[test]
    fn extract_respects_max() {
        assert_eq!(extract_titles(FIXTURE, 2).len(), 2);
    }

    #[test]
    fn score_counts_votes_over_titles() {
        // 2 bullish (surge-record + rebound-upgrade), 1 bearish, 1 neutral.
        let titles = extract_titles(FIXTURE, 30);
        let s = score_titles(&titles);
        assert!((s - 0.25).abs() < 1e-6, "got {s}");
    }

    #[test]
    fn score_empty_is_neutral() {
        assert_eq!(score_titles(&[]), 0.0);
    }

    #[test]
    fn score_extremes() {
        let bull = vec!["rally continues".to_string(), "bull market gains".to_string()];
        assert!(score_titles(&bull) > 0.9);
        let bear = vec!["crash fears".to_string(), "hack confirmed".to_string()];
        assert!(score_titles(&bear) < -0.9);
    }

    #[tokio::test]
    async fn sentiment_from_dead_feeds_is_neutral() {
        let s = fetch_sentiment_from(&["http://127.0.0.1:9/rss"]).await;
        assert_eq!(s, 0.0);
    }

    #[test]
    fn parse_fear_greed_fixture() {
        let body: serde_json::Value = serde_json::from_str(
            r#"{"data":[{"value":"28","value_classification":"Fear"}]}"#,
        )
        .unwrap();
        let v: u8 = body["data"][0]["value"].as_str().unwrap().parse().unwrap();
        assert_eq!(v, 28);
        // 28 maps bearish.
        assert!((combine_sentiment(0.0, Some(28)) + 0.22).abs() < 1e-6);
        // 50 is neutral, missing F&G keeps RSS alone.
        assert!((combine_sentiment(0.4, Some(50)) - 0.2).abs() < 1e-6);
        assert_eq!(combine_sentiment(-0.6, None), -0.6);
        assert!((combine_sentiment(1.0, Some(100)) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn parse_funding_fixture() {
        let body: serde_json::Value = serde_json::from_str(
            r#"[{"symbol":"BTCUSDT","fundingRate":"0.00009043"}]"#,
        )
        .unwrap();
        let r: f64 = body[0]["fundingRate"].as_str().unwrap().parse().unwrap();
        assert!((r - 0.00009043).abs() < 1e-10);
    }

    #[tokio::test]
    async fn fear_greed_dead_endpoint_is_none() {
        // Unreachable port: must degrade, never panic. Uses a local
        // closed port so the test stays hermetic.
        let v = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap()
            .get("http://127.0.0.1:9/fng/")
            .send()
            .await
            .ok();
        assert!(v.is_none());
    }
}
