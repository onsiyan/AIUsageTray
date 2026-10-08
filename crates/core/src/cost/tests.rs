use std::fs;
use std::io::Write;
use std::path::Path;

use chrono::{Days, Local, TimeZone};

use super::pricing::{BUNDLED_PRICES, model_keys, models_dev_prices};
use super::scan::{CACHE_FILE, for_each_line};
use super::*;

/// Noon, `days_ago` days before today, as Codex and Claude write it.
fn stamp(days_ago: u64) -> String {
    let day = Local::now().date_naive() - Days::new(days_ago);
    Local
        .from_local_datetime(&day.and_hms_opt(12, 0, 0).unwrap())
        .unwrap()
        .to_rfc3339()
}

fn token_count(time: &str, total: u64, input: u64, cached: u64, output: u64) -> String {
    format!(
        r#"{{"timestamp":"{time}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":{total},"output_tokens":0,"total_tokens":{total}}},"last_token_usage":{{"input_tokens":{input},"cached_input_tokens":{cached},"output_tokens":{output},"reasoning_output_tokens":1,"total_tokens":{}}}}}}}}}"#,
        input + output
    )
}

fn turn_context(time: &str, model: &str) -> String {
    format!(
        r#"{{"timestamp":"{time}","type":"turn_context","payload":{{"cwd":"C:\\x","model":"{model}"}}}}"#
    )
}

fn claude_line(time: &str, id: &str, request: &str, output: u64) -> String {
    format!(
        r#"{{"parentUuid":"p","type":"assistant","timestamp":"{time}","requestId":"{request}","message":{{"id":"{id}","model":"claude-sonnet-5-5","usage":{{"input_tokens":10,"cache_creation_input_tokens":1000,"cache_read_input_tokens":2000,"output_tokens":{output},"cache_creation":{{"ephemeral_1h_input_tokens":400,"ephemeral_5m_input_tokens":600}}}}}}}}"#
    )
}

fn write_lines(path: &Path, lines: &[String]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    for line in lines {
        writeln!(file, "{line}").unwrap();
    }
}

fn roots(home: &Path) -> LogRoots {
    LogRoots {
        codex: vec![home.join("codex")],
        claude: vec![home.join("claude")],
    }
}

#[test]
fn codex_counts_each_turn_once_and_follows_the_model() {
    let home = tempfile::tempdir().unwrap();
    let cache = home.path().join("cache");
    let today = stamp(0);
    let session = home.path().join("codex/2026/10/07/rollout-a.jsonl");
    write_lines(
        &session,
        &[
            turn_context(&today, "gpt-5.6-luna"),
            // A forked session's running total starts from its parent's.
            token_count(&today, 209_000_000, 1_000, 400, 100),
            // The same event again: nothing new.
            token_count(&today, 209_000_000, 1_000, 400, 100),
            token_count(&today, 209_001_100, 1_000, 0, 100),
            turn_context(&today, "gpt-5.5"),
            token_count(&today, 209_400_000, 300_000, 0, 10),
            token_count(&stamp(60), 209_500_000, 5, 0, 5),
        ],
    );

    let report = scan_report(&roots(home.path()), &cache);
    let codex = report.tool(CostTool::Codex).unwrap();
    assert!(codex.logs_found);
    // The report reaches back to the oldest event, past its usual 30 days.
    assert_eq!(codex.days.len(), 61);
    let today_cost = codex.last(1);
    assert_eq!(today_cost.tokens.cache_read, 400);
    assert_eq!(today_cost.tokens.input, 600 + 1_000 + 300_000);
    assert_eq!(today_cost.tokens.output, 210);
    assert_eq!(today_cost.tokens.reasoning, 3);
    assert_eq!(today_cost.unpriced_tokens, 0);
    let luna = (600.0 * 0.2 + 400.0 * 0.02 + 100.0 * 1.2 + 1_000.0 * 0.2 + 100.0 * 1.2) / 1e6;
    // Past 272k input tokens, gpt-5.5's long-context rates apply.
    let long = (300_000.0 * 10.0 + 10.0 * 45.0) / 1e6;
    assert!((today_cost.cost_usd - (luna + long)).abs() < 1e-9);
    let models = codex.models(1);
    assert_eq!(models[0].model, "gpt-5.5");
    assert_eq!(models.len(), 2);
    // 400 cached tokens at $0.20 rather than $0.02 per million.
    assert!((today_cost.cache_savings_usd - 400.0 * 0.18 / 1e6).abs() < 1e-12);
    assert_eq!(codex.models(30).len(), 2);
    assert_eq!(codex.last(61).tokens.output, 215);
}

#[test]
fn codex_reads_only_what_was_added_and_waits_for_unfinished_lines() {
    let home = tempfile::tempdir().unwrap();
    let cache = home.path().join("cache");
    let today = stamp(0);
    let session = home.path().join("codex/rollout-b.jsonl");
    write_lines(
        &session,
        &[
            turn_context(&today, "gpt-5.6-luna"),
            token_count(&today, 100, 100, 0, 0),
        ],
    );
    let first = scan_report(&roots(home.path()), &cache);
    assert_eq!(
        first.tool(CostTool::Codex).unwrap().last(1).tokens.input,
        100
    );
    assert!(cache.join(CACHE_FILE).is_file());

    // A line still being written is left for the next scan.
    let next = token_count(&today, 150, 50, 0, 0);
    let (written, rest) = next.split_at(40);
    let mut file = fs::OpenOptions::new().append(true).open(&session).unwrap();
    file.write_all(written.as_bytes()).unwrap();
    drop(file);
    let partial = scan_report(&roots(home.path()), &cache);
    assert_eq!(
        partial.tool(CostTool::Codex).unwrap().last(1).tokens.input,
        100
    );

    let mut file = fs::OpenOptions::new().append(true).open(&session).unwrap();
    writeln!(file, "{rest}").unwrap();
    drop(file);
    let finished = scan_report(&roots(home.path()), &cache);
    assert_eq!(
        finished.tool(CostTool::Codex).unwrap().last(1).tokens.input,
        150
    );
    // Unchanged files give the same answer from the cache alone.
    let again = scan_report(&roots(home.path()), &cache);
    assert_eq!(
        again.tool(CostTool::Codex).unwrap().last(1).tokens.input,
        150
    );
}

#[test]
fn claude_counts_a_repeated_response_once_and_prices_hour_long_cache_writes() {
    let home = tempfile::tempdir().unwrap();
    let cache = home.path().join("cache");
    let today = stamp(0);
    write_lines(
        &home.path().join("claude/project/a.jsonl"),
        &[
            claude_line(&today, "msg_1", "req_1", 100),
            claude_line(&today, "msg_1", "req_1", 300),
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#.to_owned(),
        ],
    );
    write_lines(
        &home.path().join("claude/project/b.jsonl"),
        &[
            claude_line(&today, "msg_1", "req_1", 300),
            claude_line(&stamp(3), "msg_2", "req_2", 50),
        ],
    );

    let report = scan_report(&roots(home.path()), &cache);
    let claude = report.tool(CostTool::Claude).unwrap();
    let today_cost = claude.last(1);
    assert_eq!(today_cost.tokens.output, 300);
    assert_eq!(today_cost.tokens.cache_write, 1_000);
    assert_eq!(today_cost.tokens.cache_write_1h, 400);
    // Sonnet 5.5: $2 input, $10 output, $0.20 cache read, $2.50 cache write.
    let expected = (10.0 * 2.0 + 2_000.0 * 0.2 + 600.0 * 2.5 + 400.0 * 4.0 + 300.0 * 10.0) / 1e6;
    assert!((today_cost.cost_usd - expected).abs() < 1e-9);
    assert_eq!(claude.last(7).tokens.output, 350);
    assert!(!report.tool(CostTool::Codex).unwrap().logs_found);
}

#[test]
fn unknown_models_are_counted_but_not_priced() {
    let home = tempfile::tempdir().unwrap();
    let cache = home.path().join("cache");
    let today = stamp(0);
    write_lines(
        &home.path().join("codex/rollout-c.jsonl"),
        &[
            turn_context(&today, "house-model-1"),
            token_count(&today, 10, 10, 0, 0),
        ],
    );
    let report = scan_report(&roots(home.path()), &cache);
    let codex = report.tool(CostTool::Codex).unwrap();
    assert_eq!(codex.last(1).unpriced_tokens, 10);
    assert_eq!(codex.last(1).cost_usd, 0.0);
    assert_eq!(codex.models(1)[0].cost_usd, None);
}

#[test]
fn long_lines_are_skipped_without_being_kept() {
    let mut text = Vec::new();
    text.extend_from_slice(br#"{"type":"compacted","payload":""#);
    text.extend(std::iter::repeat_n(b'x', 5_000));
    text.extend_from_slice(b"\"}\n");
    text.extend_from_slice(b"{\"type\":\"turn_context\"}\n");
    text.extend_from_slice(b"{\"type\":\"turn");
    let mut seen = Vec::new();
    let consumed = for_each_line(
        std::io::BufReader::with_capacity(64, text.as_slice()),
        24,
        |prefix| prefix.windows(12).any(|window| window == b"turn_context"),
        |line| seen.push(String::from_utf8_lossy(line).into_owned()),
    )
    .unwrap();
    assert_eq!(seen, vec![r#"{"type":"turn_context"}"#]);
    assert_eq!(consumed as usize, text.len() - b"{\"type\":\"turn".len());
}

#[test]
fn model_names_drop_tags_prefixes_and_dates() {
    assert_eq!(
        model_keys("claude-haiku-4-5-20251001"),
        vec![
            "claude-haiku-4-5-20251001".to_owned(),
            "claude-haiku-4-5".to_owned()
        ]
    );
    assert_eq!(
        model_keys("anthropic/Claude-Opus-5-5[1m]"),
        vec!["claude-opus-5-5".to_owned()]
    );
}

#[test]
fn models_dev_prices_keep_rates_and_context_tiers() {
    let catalog = serde_json::json!({
        "openai": {"models": {"gpt-x": {"cost": {
            "input": 1.0, "output": 8.0, "cache_read": 0.1,
            "tiers": [{"input": 2.0, "output": 12.0, "tier": {"type": "context", "size": 272000}}]
        }}, "free-text": {"name": "no cost"}}},
        "anthropic": {"models": {"claude-x": {"cost": {"input": 3.0, "output": 15.0}}}},
        "other": {"models": {"m": {"cost": {"input": 1.0, "output": 1.0}}}}
    });
    let prices = models_dev_prices(&catalog);
    assert_eq!(prices.len(), 2);
    let file = serde_json::json!({"models": prices});
    assert_eq!(file["models"]["openai/gpt-x"]["long"]["threshold"], 272000);
    assert_eq!(file["models"]["anthropic/claude-x"]["output"], 15.0);
}

#[test]
fn bundled_prices_cover_the_current_models() {
    let table = PriceTable::load(Path::new("does-not-exist"));
    assert!(!table.source.downloaded);
    assert!(BUNDLED_PRICES.contains("openai/gpt-5.6-luna"));
    let tokens = TokenCounts {
        input: 1_000_000,
        ..TokenCounts::default()
    };
    for (tool, model) in [
        (CostTool::Codex, "gpt-5.6-sol"),
        (CostTool::Codex, "gpt-6-astra"),
        (CostTool::Claude, "claude-opus-5-5"),
        (CostTool::Claude, "claude-fable-5-1"),
        (CostTool::Claude, "claude-haiku-4-5-20251001"),
    ] {
        assert!(table.cost(tool, model, &tokens, false).is_some(), "{model}");
    }
}

#[test]
fn other_machines_join_the_report_on_this_pcs_calendar() {
    let home = tempfile::tempdir().unwrap();
    write_lines(
        &home.path().join("claude").join("p").join("a.jsonl"),
        &[claude_line(&stamp(0), "m1", "r1", 50)],
    );
    let noon = DateTime::parse_from_rfc3339(&stamp(1)).unwrap().timestamp();
    let row = |slot: i64, output: u64| remote::RemoteRow {
        tool: CostTool::Codex,
        slot,
        model: "gpt-5.5".to_owned(),
        long_context: false,
        tokens: TokenCounts {
            input: 1_000,
            output,
            ..TokenCounts::default()
        },
    };
    let reading = remote::RemoteUsage {
        found: vec![CostTool::Codex],
        // Yesterday's noon, and 40 days before it.
        rows: vec![row(noon - noon % 1800, 10), row(noon - 40 * 86_400, 99)],
        synced_at: Utc::now(),
        token: None,
    };
    let report = scan_report_with(
        &roots(home.path()),
        &home.path().join("cache"),
        &[("vps".to_owned(), reading)],
    );

    assert_eq!(report.machines.len(), 2);
    assert_eq!(report.machines[0].name, None);
    assert_eq!(report.machines[1].name.as_deref(), Some("vps"));
    let codex = report.tool(CostTool::Codex).unwrap();
    assert!(codex.logs_found);
    assert_eq!(codex.last(2).tokens.output, 10);
    assert_eq!(codex.last(1).tokens.output, 0);
    let remote_codex = &report.machines[1].tools[0];
    assert_eq!(remote_codex.last(30).tokens.input, 1_000);
    assert_eq!(remote_codex.last(42).tokens.output, 109);
    assert_eq!(codex.days.len(), 42);
    assert!(report.machines[1].cost_over(30) > 0.0);
    assert_eq!(report.machines[0].tools[0].last(30).tokens.total(), 0);
    // Only this PC: no machine list.
    assert!(
        scan_report(&roots(home.path()), &home.path().join("cache"))
            .machines
            .is_empty()
    );
}

#[test]
fn usage_outlives_the_logs_it_was_read_from() {
    let home = tempfile::tempdir().unwrap();
    let cache = home.path().join("cache");
    let old = home.path().join("codex/rollout-old.jsonl");
    write_lines(
        &old,
        &[
            turn_context(&stamp(100), "gpt-5.5"),
            token_count(&stamp(100), 100, 100, 0, 7),
        ],
    );
    let today = home.path().join("codex/rollout-new.jsonl");
    write_lines(
        &today,
        &[
            turn_context(&stamp(0), "gpt-5.5"),
            token_count(&stamp(0), 50, 50, 0, 3),
        ],
    );
    let first = scan_report(&roots(home.path()), &cache);
    assert_eq!(first.tool(CostTool::Codex).unwrap().days.len(), 101);

    // Codex clears the old session, and the new one grows.
    fs::remove_file(&old).unwrap();
    write_lines(&today, &[token_count(&stamp(0), 80, 30, 0, 2)]);
    let later = scan_report(&roots(home.path()), &cache);
    let codex = later.tool(CostTool::Codex).unwrap();
    assert_eq!(codex.days.len(), 101);
    assert_eq!(codex.last(101).tokens.output, 12);
    assert_eq!(codex.last(1).tokens.output, 5);
    assert!(cache.join(history::HISTORY_FILE).is_file());
}
