//! Provider sign-in helper. The CLI runs `usage-monitor-login <provider>
//! [options]` to add or re-authenticate an account; each provider's flow
//! lives in its own module and reads its options after the provider name.

mod antigravity;
mod api_key;
mod claude;
mod codex;
mod copilot;
mod cursor;
mod deepseek;
mod opencode_go;
mod openrouter;

const USAGE: &str = "Usage: usage-monitor-login <codex|claude|antigravity|opencode-go|openrouter|deepseek|copilot|cursor|kimi|zai> [options]";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match std::env::args().nth(1).as_deref() {
        Some("codex") => codex::run().await,
        Some("claude") => claude::run().await,
        Some("antigravity") => antigravity::run().await,
        Some("opencode-go") => opencode_go::run().await,
        Some("openrouter") => openrouter::run().await,
        Some("deepseek") => deepseek::run().await,
        Some("copilot") => copilot::run().await,
        Some("cursor") => cursor::run().await,
        Some("kimi") => api_key::run(&api_key::KIMI_CODE).await,
        Some("zai") => api_key::run(&api_key::ZAI_CODING_PLAN).await,
        Some("--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}
