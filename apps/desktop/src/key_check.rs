//! Tests a saved key against its service: one free request that needs the
//! key (listing models, or reading the account), so nothing is spent.

use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(15);

/// How a service takes its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Auth {
    Bearer,
    Header(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Check {
    url: &'static str,
    auth: Auth,
    extra_header: Option<(&'static str, &'static str)>,
}

const fn bearer(url: &'static str) -> Check {
    Check {
        url,
        auth: Auth::Bearer,
        extra_header: None,
    }
}

/// What the test found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Works,
    /// The service turned the key down.
    Rejected,
    /// Anything else: the service's answer or why it could not be reached.
    Failed(String),
}

/// The check for each service, by the name the Keys page gives it. Only
/// endpoints that refuse a request without a valid key are listed, so an
/// answer always says something about the key.
const CHECKS: &[(&str, Check)] = &[
    ("OpenAI", bearer("https://api.openai.com/v1/models")),
    (
        "Claude",
        Check {
            url: "https://api.anthropic.com/v1/models",
            auth: Auth::Header("x-api-key"),
            extra_header: Some(("anthropic-version", "2023-06-01")),
        },
    ),
    (
        "Google Gemini",
        Check {
            url: "https://generativelanguage.googleapis.com/v1beta/models",
            auth: Auth::Header("x-goog-api-key"),
            extra_header: None,
        },
    ),
    ("xAI", bearer("https://api.x.ai/v1/models")),
    ("DeepSeek", bearer("https://api.deepseek.com/models")),
    ("Mistral", bearer("https://api.mistral.ai/v1/models")),
    ("OpenRouter", bearer("https://openrouter.ai/api/v1/key")),
    ("Groq", bearer("https://api.groq.com/openai/v1/models")),
    ("Kimi", bearer("https://api.moonshot.ai/v1/models")),
    ("Cohere", bearer("https://api.cohere.com/v1/models")),
    ("Together AI", bearer("https://api.together.xyz/v1/models")),
    (
        "Fireworks AI",
        bearer("https://api.fireworks.ai/inference/v1/models"),
    ),
    ("Cerebras", bearer("https://api.cerebras.ai/v1/models")),
    (
        "Hugging Face",
        bearer("https://huggingface.co/api/whoami-v2"),
    ),
    (
        "ElevenLabs",
        Check {
            url: "https://api.elevenlabs.io/v1/user",
            auth: Auth::Header("xi-api-key"),
            extra_header: None,
        },
    ),
    ("GitHub", bearer("https://api.github.com/user")),
    ("Replicate", bearer("https://api.replicate.com/v1/account")),
    (
        "Stability AI",
        bearer("https://api.stability.ai/v1/user/account"),
    ),
];

fn check_for(service: &str) -> Option<Check> {
    CHECKS
        .iter()
        .find(|(name, _)| *name == service)
        .map(|(_, check)| *check)
}

pub(crate) fn can_test(service: &str) -> bool {
    check_for(service).is_some()
}

/// Runs the test on its own thread, off the interface.
pub(crate) async fn test(service: String, secret: String) -> Outcome {
    let Some(check) = check_for(&service) else {
        return Outcome::Failed("no test for this service".to_owned());
    };
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = sender.send_blocking(run(check, &secret));
    });
    receiver
        .recv()
        .await
        .unwrap_or_else(|_| Outcome::Failed("the test stopped".to_owned()))
}

fn run(check: Check, secret: &str) -> Outcome {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return Outcome::Failed(error.to_string()),
    };
    let client = match reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("AI-Usage-Tray/", env!("CARGO_PKG_VERSION")))
        .build()
    {
        Ok(client) => client,
        Err(error) => return Outcome::Failed(error.to_string()),
    };
    let secret = secret.trim();
    let mut request = client.get(check.url);
    request = match check.auth {
        Auth::Bearer => request.bearer_auth(secret),
        Auth::Header(name) => request.header(name, secret),
    };
    if let Some((name, value)) = check.extra_header {
        request = request.header(name, value);
    }
    match runtime.block_on(request.send()) {
        Ok(response) => outcome(response.status().as_u16()),
        Err(error) if error.is_timeout() => Outcome::Failed("no answer in time".to_owned()),
        Err(error) if error.is_connect() => Outcome::Failed("could not connect".to_owned()),
        Err(error) => Outcome::Failed(error.without_url().to_string()),
    }
}

fn outcome(status: u16) -> Outcome {
    match status {
        200..=299 => Outcome::Works,
        // Rate limited: the key was accepted.
        429 => Outcome::Works,
        400 | 401 | 403 => Outcome::Rejected,
        status => Outcome::Failed(format!("HTTP {status}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_say_whether_the_key_works() {
        assert_eq!(outcome(200), Outcome::Works);
        assert_eq!(outcome(429), Outcome::Works);
        assert_eq!(outcome(401), Outcome::Rejected);
        assert_eq!(outcome(403), Outcome::Rejected);
        assert_eq!(outcome(503), Outcome::Failed("HTTP 503".to_owned()));
    }

    #[test]
    fn every_check_names_a_listed_service() {
        for (name, check) in CHECKS {
            assert!(crate::key_services::find(name).is_some(), "{name}");
            assert!(check.url.starts_with("https://"), "{name}");
        }
        assert!(can_test("OpenAI"));
        assert!(!can_test("Some service nobody lists"));
    }
}
