//! The key vault: API keys the user keeps in the app to copy later, for any
//! service, whether or not the app reads its usage. Each key is stored whole
//! (name, service, variable name, and the key) in the platform's secret
//! store; nothing about it is written to the account database.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Longest name, service, or variable name kept, in characters.
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
    /// The environment variable the key is usually set as, such as
    /// `OPENAI_API_KEY`; empty when there is none.
    #[serde(default)]
    pub env_var: String,
    pub secret: String,
    pub created_at: DateTime<Utc>,
}

impl VaultKey {
    /// A new key with a fresh id. Fails when the name or key is empty or
    /// too long; the service and variable name are tidied.
    pub fn new(name: &str, service: &str, env_var: &str, secret: &str) -> Result<Self, String> {
        let mut key = Self {
            id: Uuid::new_v4().simple().to_string(),
            name: String::new(),
            service: String::new(),
            env_var: String::new(),
            secret: String::new(),
            created_at: Utc::now(),
        };
        key.edit(name, service, env_var, secret)?;
        Ok(key)
    }

    /// Replaces the key's details, keeping its id and creation time.
    pub fn edit(
        &mut self,
        name: &str,
        service: &str,
        env_var: &str,
        secret: &str,
    ) -> Result<(), String> {
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
        self.env_var = clip(&env_var_name(env_var));
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

/// A variable name from what the user typed: upper case, with anything other
/// than letters, digits, and underscores turned into underscores.
pub fn env_var_name(text: &str) -> String {
    let name: String = text
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let name = name.trim_matches('_').to_owned();
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        format!("_{name}")
    } else {
        name
    }
}

/// Services whose keys are told apart by how they start, with the variable
/// their tools read. Longer prefixes come first.
const KNOWN_SERVICES: &[(&str, &str, &str)] = &[
    ("sk-ant-", "Anthropic", "ANTHROPIC_API_KEY"),
    ("sk-or-", "OpenRouter", "OPENROUTER_API_KEY"),
    ("sk-proj-", "OpenAI", "OPENAI_API_KEY"),
    ("sk-svcacct-", "OpenAI", "OPENAI_API_KEY"),
    ("sk-admin-", "OpenAI", "OPENAI_ADMIN_KEY"),
    ("AIza", "Google Gemini", "GEMINI_API_KEY"),
    ("xai-", "xAI", "XAI_API_KEY"),
    ("gsk_", "Groq", "GROQ_API_KEY"),
    ("pplx-", "Perplexity", "PERPLEXITY_API_KEY"),
    ("hf_", "Hugging Face", "HF_TOKEN"),
    ("github_pat_", "GitHub", "GITHUB_TOKEN"),
    ("ghp_", "GitHub", "GITHUB_TOKEN"),
    ("r8_", "Replicate", "REPLICATE_API_TOKEN"),
    ("fw_", "Fireworks AI", "FIREWORKS_API_KEY"),
];

/// Variables for services named by the user, matched case-insensitively.
const SERVICE_VARIABLES: &[(&str, &str)] = &[
    ("anthropic", "ANTHROPIC_API_KEY"),
    ("claude", "ANTHROPIC_API_KEY"),
    ("openai", "OPENAI_API_KEY"),
    ("chatgpt", "OPENAI_API_KEY"),
    ("openrouter", "OPENROUTER_API_KEY"),
    ("gemini", "GEMINI_API_KEY"),
    ("google gemini", "GEMINI_API_KEY"),
    ("google", "GEMINI_API_KEY"),
    ("deepseek", "DEEPSEEK_API_KEY"),
    ("xai", "XAI_API_KEY"),
    ("grok", "XAI_API_KEY"),
    ("groq", "GROQ_API_KEY"),
    ("mistral", "MISTRAL_API_KEY"),
    ("perplexity", "PERPLEXITY_API_KEY"),
    ("together", "TOGETHER_API_KEY"),
    ("together ai", "TOGETHER_API_KEY"),
    ("fireworks", "FIREWORKS_API_KEY"),
    ("fireworks ai", "FIREWORKS_API_KEY"),
    ("cohere", "COHERE_API_KEY"),
    ("kimi", "MOONSHOT_API_KEY"),
    ("moonshot", "MOONSHOT_API_KEY"),
    ("z.ai", "ZAI_API_KEY"),
    ("zai", "ZAI_API_KEY"),
    ("minimax", "MINIMAX_API_KEY"),
    ("hugging face", "HF_TOKEN"),
    ("huggingface", "HF_TOKEN"),
    ("github", "GITHUB_TOKEN"),
    ("replicate", "REPLICATE_API_TOKEN"),
    ("elevenlabs", "ELEVENLABS_API_KEY"),
    ("qwen", "DASHSCOPE_API_KEY"),
    ("dashscope", "DASHSCOPE_API_KEY"),
];

/// The service a pasted key belongs to, when its start says so.
pub fn detect_service(secret: &str) -> Option<&'static str> {
    let secret = secret.trim();
    KNOWN_SERVICES
        .iter()
        .find(|(prefix, ..)| secret.starts_with(prefix))
        .map(|(_, service, _)| *service)
}

/// The variable usually used for `service`'s key: a known one, else
/// `<SERVICE>_API_KEY`. Empty when no service is given.
pub fn suggested_env_var(service: &str) -> String {
    let service = service.trim();
    if service.is_empty() {
        return String::new();
    }
    let lower = service.to_lowercase();
    if let Some((_, variable)) = SERVICE_VARIABLES.iter().find(|(name, _)| *name == lower) {
        return (*variable).to_owned();
    }
    if let Some((.., variable)) = KNOWN_SERVICES
        .iter()
        .find(|(_, name, _)| name.eq_ignore_ascii_case(service))
    {
        return (*variable).to_owned();
    }
    let base = env_var_name(service);
    if base.is_empty() {
        String::new()
    } else {
        format!("{base}_API_KEY")
    }
}

/// How a copied key is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    /// The key alone.
    Plain,
    /// `$env:NAME = "key"`, for PowerShell.
    PowerShell,
    /// `export NAME='key'`, for bash and zsh.
    Posix,
    /// `NAME=key`, for a `.env` file.
    DotEnv,
}

/// `secret` laid out as `format` asks, with `env_var` as the variable name.
/// Keys never hold quotes or control characters a shell would read, but the
/// quoting is still made safe for any text.
pub fn copy_text(format: CopyFormat, env_var: &str, secret: &str) -> String {
    let variable = if env_var.is_empty() {
        "API_KEY"
    } else {
        env_var
    };
    match format {
        CopyFormat::Plain => secret.to_owned(),
        CopyFormat::PowerShell => {
            // Single quotes take everything literally; a quote is doubled.
            format!("$env:{variable} = '{}'", secret.replace('\'', "''"))
        }
        CopyFormat::Posix => format!("export {variable}='{}'", secret.replace('\'', r"'\''")),
        CopyFormat::DotEnv => format!("{variable}={secret}"),
    }
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

/// Whether `key` matches what the user searched for, by name, service, or
/// variable name (never by the key itself).
pub fn matches(key: &VaultKey, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || [&key.name, &key.service, &key.env_var]
            .iter()
            .any(|field| field.to_lowercase().contains(&query))
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
        assert_eq!(detect_service("sk-ant-api03-x"), Some("Anthropic"));
        assert_eq!(detect_service("  sk-or-v1-x"), Some("OpenRouter"));
        assert_eq!(detect_service("sk-proj-x"), Some("OpenAI"));
        assert_eq!(detect_service("AIzaSy"), Some("Google Gemini"));
        assert_eq!(detect_service("xai-abc"), Some("xAI"));
        assert_eq!(detect_service("gsk_abc"), Some("Groq"));
        // Plain `sk-` keys are used by OpenAI, DeepSeek and others alike.
        assert_eq!(detect_service("sk-abc"), None);
    }

    #[test]
    fn variables_follow_the_service() {
        assert_eq!(suggested_env_var("OpenAI"), "OPENAI_API_KEY");
        assert_eq!(suggested_env_var("Google Gemini"), "GEMINI_API_KEY");
        assert_eq!(suggested_env_var("  deepseek "), "DEEPSEEK_API_KEY");
        assert_eq!(suggested_env_var("Hugging Face"), "HF_TOKEN");
        assert_eq!(suggested_env_var("My Service 2"), "MY_SERVICE_2_API_KEY");
        assert_eq!(suggested_env_var(""), "");
        assert_eq!(env_var_name(" open-ai key "), "OPEN_AI_KEY");
        assert_eq!(env_var_name("1password"), "_1PASSWORD");
    }

    #[test]
    fn copies_are_safe_to_paste_into_a_shell() {
        assert_eq!(copy_text(CopyFormat::Plain, "X", "k"), "k");
        assert_eq!(
            copy_text(CopyFormat::PowerShell, "OPENAI_API_KEY", "sk-1"),
            "$env:OPENAI_API_KEY = 'sk-1'"
        );
        assert_eq!(
            copy_text(CopyFormat::Posix, "", "a'b"),
            r"export API_KEY='a'\''b'"
        );
        assert_eq!(
            copy_text(CopyFormat::PowerShell, "K", "a'b"),
            "$env:K = 'a''b'"
        );
        assert_eq!(copy_text(CopyFormat::DotEnv, "K", "v"), "K=v");
    }

    #[test]
    fn new_keys_are_checked_and_tidied() {
        let key = VaultKey::new("  Work ", " OpenAI ", "openai api key", " sk-proj-1 ").unwrap();
        assert_eq!(key.name, "Work");
        assert_eq!(key.service, "OpenAI");
        assert_eq!(key.env_var, "OPENAI_API_KEY");
        assert_eq!(key.secret, "sk-proj-1");
        assert_eq!(key.id.len(), 32);
        assert!(VaultKey::new("", "", "", "k").is_err());
        assert!(VaultKey::new("n", "", "", "  ").is_err());
        assert!(VaultKey::new("n", "", "", "a\nb").is_err());
        assert!(VaultKey::new("n", "", "", &"k".repeat(MAX_SECRET_BYTES + 1)).is_err());
        let round = serde_json::to_string(&key).unwrap();
        assert_eq!(serde_json::from_str::<VaultKey>(&round).unwrap(), key);
    }

    #[test]
    fn search_never_looks_at_the_key() {
        let key = VaultKey::new("Personal", "Anthropic", "", "sk-ant-secret").unwrap();
        assert!(matches(&key, "pers"));
        assert!(matches(&key, "ANTHROP"));
        assert!(matches(&key, ""));
        assert!(!matches(&key, "secret"));
    }
}
