//! The key vault: API keys the user keeps in the app to copy later, for any
//! service, whether or not the app reads its usage. Each key is stored whole
//! (name, service, and the key) in the platform's secret store; nothing about
//! it is written to the account database.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Longest name or service kept, in characters.
pub const MAX_LABEL_CHARS: usize = 64;
/// Longest key kept, in bytes: far beyond any provider's keys, and within
/// what the secret store holds for one entry.
pub const MAX_SECRET_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultKey {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub service: String,
    pub secret: String,
    pub created_at: DateTime<Utc>,
}

impl VaultKey {
    /// A new key with a fresh id. Fails when the name or key is empty or
    /// too long; the service is tidied.
    pub fn new(name: &str, service: &str, secret: &str) -> Result<Self, String> {
        let mut key = Self {
            id: Uuid::new_v4().simple().to_string(),
            name: String::new(),
            service: String::new(),
            secret: String::new(),
            created_at: Utc::now(),
        };
        key.edit(name, service, secret)?;
        Ok(key)
    }

    /// Replaces the key's details, keeping its id and creation time.
    pub fn edit(&mut self, name: &str, service: &str, secret: &str) -> Result<(), String> {
        let name = name.trim();
        let secret = secret.trim();
        if name.is_empty() {
            return Err("The key needs a name.".to_owned());
        }
        if secret.is_empty() {
            return Err("The key is empty.".to_owned());
        }
        if secret.len() > MAX_SECRET_BYTES {
            return Err("The key is too long to keep.".to_owned());
        }
        if secret.chars().any(char::is_control) {
            return Err("The key holds line breaks or other control characters.".to_owned());
        }
        self.name = clip(name);
        self.service = clip(service.trim());
        self.secret = secret.to_owned();
        Ok(())
    }

    /// The key with its middle hidden, for showing in a list.
    pub fn masked(&self) -> String {
        masked(&self.secret)
    }
}

fn clip(text: &str) -> String {
    text.chars().take(MAX_LABEL_CHARS).collect()
}

/// `secret` with only its start and end shown: `sk-or-v1…a1b2`. Short keys
/// show less, so that most of the key always stays hidden.
pub fn masked(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    let (head, tail) = match chars.len() {
        0..=8 => (0, 0),
        9..=16 => (2, 2),
        17..=24 => (4, 4),
        _ => (8.min(prefix_length(&chars) + 4), 4),
    };
    let start: String = chars[..head].iter().collect();
    let end: String = chars[chars.len() - tail..].iter().collect();
    format!("{start}••••{end}")
}

/// Length of a key's readable prefix, such as `sk-ant-`, up to its last dash
/// or underscore within the first 8 characters.
fn prefix_length(chars: &[char]) -> usize {
    chars
        .iter()
        .take(8)
        .rposition(|c| matches!(c, '-' | '_'))
        .map_or(0, |index| index + 1)
}

/// Services whose keys are told apart by how they start. Longer prefixes
/// come first.
const KNOWN_SERVICES: &[(&str, &str)] = &[
    ("sk-ant-", "Claude"),
    ("sk-or-", "OpenRouter"),
    ("sk-proj-", "OpenAI"),
    ("sk-svcacct-", "OpenAI"),
    ("sk-admin-", "OpenAI"),
    ("AIza", "Google Gemini"),
    ("xai-", "xAI"),
    ("gsk_", "Groq"),
    ("pplx-", "Perplexity"),
    ("hf_", "Hugging Face"),
    ("github_pat_", "GitHub"),
    ("ghp_", "GitHub"),
    ("r8_", "Replicate"),
    ("fw_", "Fireworks AI"),
];

/// The service a pasted key belongs to, when its start says so.
pub fn detect_service(secret: &str) -> Option<&'static str> {
    let secret = secret.trim();
    KNOWN_SERVICES
        .iter()
        .find(|(prefix, _)| secret.starts_with(prefix))
        .map(|(_, service)| *service)
}

/// Keys in list order: by service, then name, ignoring case.
pub fn sort(keys: &mut [VaultKey]) {
    keys.sort_by(|a, b| {
        a.service
            .to_lowercase()
            .cmp(&b.service.to_lowercase())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.created_at.cmp(&b.created_at))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_hides_most_of_any_key() {
        assert_eq!(
            masked("sk-ant-api03-abcdefghijklmnopqrstuvwxyz1234"),
            "sk-ant-a••••1234"
        );
        assert_eq!(
            masked("sk-or-v1-0123456789abcdef0123456789"),
            "sk-or-v1••••6789"
        );
        assert_eq!(masked("AIzaSyA0123456789abcdefghijklmnop"), "AIza••••mnop");
        assert_eq!(masked("abcdefghijklmnopqrst"), "abcd••••qrst");
        assert_eq!(masked("abcdefghijkl"), "ab••••kl");
        assert_eq!(masked("short"), "••••");
        assert_eq!(masked(""), "••••");
        // Characters, not bytes, so other scripts never split.
        assert_eq!(masked("مفتاح-سري-طويل-جدا-للتجربة-هنا"), "مفتاح-سر••••-هنا");
    }

    #[test]
    fn services_are_told_by_their_key_prefix() {
        assert_eq!(detect_service("sk-ant-api03-x"), Some("Claude"));
        assert_eq!(detect_service("  sk-or-v1-x"), Some("OpenRouter"));
        assert_eq!(detect_service("sk-proj-x"), Some("OpenAI"));
        assert_eq!(detect_service("AIzaSy"), Some("Google Gemini"));
        assert_eq!(detect_service("xai-abc"), Some("xAI"));
        assert_eq!(detect_service("gsk_abc"), Some("Groq"));
        // Plain `sk-` keys are used by OpenAI, DeepSeek and others alike.
        assert_eq!(detect_service("sk-abc"), None);
    }

    #[test]
    fn new_keys_are_checked_and_tidied() {
        let key = VaultKey::new("  Work ", " OpenAI ", " sk-proj-1 ").unwrap();
        assert_eq!(key.name, "Work");
        assert_eq!(key.service, "OpenAI");
        assert_eq!(key.secret, "sk-proj-1");
        assert_eq!(key.id.len(), 32);
        assert!(VaultKey::new("", "", "k").is_err());
        assert!(VaultKey::new("n", "", "  ").is_err());
        assert!(
            VaultKey::new(
                "n", "", "a
b"
            )
            .is_err()
        );
        assert!(VaultKey::new("n", "", &"k".repeat(MAX_SECRET_BYTES + 1)).is_err());
        let round = serde_json::to_string(&key).unwrap();
        assert_eq!(serde_json::from_str::<VaultKey>(&round).unwrap(), key);
        // Keys saved with the earlier variable field still read.
        let older = round.replacen("\"secret\"", "\"env_var\":\"X\",\"secret\"", 1);
        assert_eq!(serde_json::from_str::<VaultKey>(&older).unwrap(), key);
    }
}
