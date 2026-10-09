//! Looks for a newer release on GitHub when the app starts and then once a
//! day, so the tray menu and the popup can point to it. Nothing is
//! downloaded or installed; the release page opens in the browser.

use std::time::Duration;

use iced::Task;

use crate::Message;

/// Where releases are published.
const REPOSITORY: &str = "onsiyan/AIUsageTray";
/// How long to wait before looking again.
pub(super) const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// A release newer than the running app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Release {
    /// As `0.3.0`.
    pub version: String,
    /// Its page on GitHub.
    pub url: String,
}

/// Asks GitHub for the latest release; a failure is logged and treated as
/// no news.
pub(super) fn check() -> Task<Message> {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let found = latest_release().unwrap_or_else(|error| {
            crate::app_log::write(format!("update check failed: {error}"));
            None
        });
        let _ = sender.send_blocking(found);
    });
    Task::perform(
        async move { receiver.recv().await.ok().flatten() },
        Message::UpdateChecked,
    )
}

fn latest_release() -> Result<Option<Release>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("AI-Usage-Tray/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| error.to_string())?;
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let (status, body) = runtime
        .block_on(async {
            let response = client
                .get(&url)
                .header("Accept", "application/vnd.github+json")
                .send()
                .await?;
            let status = response.status();
            Ok::<_, reqwest::Error>((status, response.text().await?))
        })
        .map_err(|error| error.to_string())?;
    // No release published yet.
    if status == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !status.is_success() {
        return Err(format!("GitHub answered {status}"));
    }
    let latest: serde_json::Value =
        serde_json::from_str(&body).map_err(|error| error.to_string())?;
    Ok(newer_release(
        latest["tag_name"].as_str().unwrap_or_default(),
        latest["html_url"].as_str().unwrap_or_default(),
        env!("CARGO_PKG_VERSION"),
    ))
}

/// The release `tag` names, if it is later than `current` and its page is
/// on this app's repository.
fn newer_release(tag: &str, url: &str, current: &str) -> Option<Release> {
    let page = format!("https://github.com/{REPOSITORY}/");
    let later =
        matches!((version(tag), version(current)), (Some(tag), Some(current)) if tag > current);
    (later && url.starts_with(&page)).then(|| Release {
        version: tag.trim().trim_start_matches('v').to_owned(),
        url: url.to_owned(),
    })
}

/// `v1.2.3` (or `1.2`, or `1.2.3-beta`) as numbers to compare.
fn version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let mut next = || parts.next().map_or(Some(0), |part| part.parse().ok());
    Some((next()?, next()?, next()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_later_release_on_this_repository_counts() {
        let page = "https://github.com/onsiyan/AIUsageTray/releases/tag/v0.3.0";
        let found = newer_release("v0.3.0", page, "0.2.0").unwrap();
        assert_eq!(found.version, "0.3.0");
        assert!(newer_release("v0.2.0", page, "0.2.0").is_none());
        assert!(newer_release("v0.1.9", page, "0.2.0").is_none());
        assert!(newer_release("v1.0", page, "0.9.9").is_some());
        assert!(newer_release("not a version", page, "0.2.0").is_none());
        assert!(newer_release("v9.0.0", "https://example.com/x", "0.2.0").is_none());
    }
}
