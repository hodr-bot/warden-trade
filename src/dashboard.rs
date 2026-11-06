use askama::Template;
use chrono::Utc;

/// Askama template for the paper-trading dashboard overview.
#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub capital_total: String,
    pub portfolio_value: String,
    pub pnl_realized: String,
    pub pnl_unrealized: String,
    pub trades_count: String,
    pub fees_paid: String,
    pub drawdown: String,
    pub win_rate: String,
    pub generated_at: String,
}

/// Backend handler building the dashboard template.
///
/// Values are backend-side placeholders; `generated_at` is always
/// the current UTC time.
pub async fn dashboard_handler() -> axum::response::Html<String> {
    let template = DashboardTemplate {
        capital_total: "0".to_string(),
        portfolio_value: "0".to_string(),
        pnl_realized: "0".to_string(),
        pnl_unrealized: "0".to_string(),
        trades_count: "0".to_string(),
        fees_paid: "0".to_string(),
        drawdown: "0".to_string(),
        win_rate: "0%".to_string(),
        generated_at: Utc::now().to_rfc3339(),
    };
    axum::response::Html(template.render().unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_template_renders_warden() {
        let template = DashboardTemplate {
            capital_total: "10000".to_string(),
            portfolio_value: "10000".to_string(),
            pnl_realized: "0".to_string(),
            pnl_unrealized: "0".to_string(),
            trades_count: "0".to_string(),
            fees_paid: "0".to_string(),
            drawdown: "0%".to_string(),
            win_rate: "0%".to_string(),
            generated_at: "now".to_string(),
        };
        let rendered = template.render().expect("template renders");
        assert!(
            rendered.contains("Warden"),
            "rendered dashboard should contain Warden"
        );
    }
}
