//! `cost`: what Codex and Claude Code usage on this PC would cost at API
//! list prices, from their local session logs.

use usage_monitor_core::cost::{self, CostReport, LogRoots};

use super::{CliFailure, print_json};

pub(super) async fn execute_cost(
    json_output: bool,
    update_prices: bool,
) -> Result<i32, CliFailure> {
    let cache_directory = cost::default_cache_directory();
    if update_prices
        && let Err(error) = cost::refresh_prices_if_stale(&cache_directory).await
        && !json_output
    {
        eprintln!("Price update failed, using the last known prices: {error}");
    }
    let report = tokio::task::spawn_blocking(move || {
        cost::scan_report(&LogRoots::from_environment(), &cache_directory)
    })
    .await
    .map_err(|error| CliFailure::runtime(error.to_string()))?;
    if json_output {
        let report = serde_json::to_value(&report)
            .map_err(|error| CliFailure::runtime(error.to_string()))?;
        print_json(serde_json::json!({ "schema_version": 1, "cost": report }));
    } else {
        print_report(&report);
    }
    Ok(0)
}

fn print_report(report: &CostReport) {
    for tool in &report.tools {
        println!("{}", tool.tool.label());
        if !tool.logs_found {
            println!("  no logs on this PC");
            continue;
        }
        for (label, days) in [
            ("Today", 1),
            ("Last 7 days", 7),
            ("Last 30 days", 30),
            ("All time", tool.days.len()),
        ] {
            let total = tool.last(days);
            let unpriced = if total.unpriced_tokens > 0 {
                format!("  ({} tokens unpriced)", total.unpriced_tokens)
            } else {
                String::new()
            };
            println!(
                "  {label:<13} ${:>10.2}  {:>15} tokens{unpriced}",
                total.cost_usd,
                total.tokens.total()
            );
        }
        for model in tool.models(tool.days.len()).iter().take(5) {
            let cost = model
                .cost_usd
                .map_or_else(|| "unpriced".to_owned(), |cost| format!("${cost:.2}"));
            println!(
                "    {:<28} {cost:>12}  {:>15} tokens",
                model.model,
                model.tokens.total()
            );
        }
    }
    let prices = if report.prices.downloaded {
        "models.dev"
    } else {
        "built-in table"
    };
    let updated = report
        .prices
        .updated_at
        .map(|time| format!(", updated {}", time.format("%Y-%m-%d %H:%M UTC")))
        .unwrap_or_default();
    println!("Prices: {prices}{updated}. Read in {} ms.", report.scan_ms);
}
