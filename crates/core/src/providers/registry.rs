//! Construction and lookup of the provider adapters used by the runtime.
//!
//! The registry is the only place that knows which concrete adapters are
//! enabled. The refresh coordinator receives the registry's trait objects and
//! remains unaware of provider constructors and provider-specific options.

use super::{
    antigravity::AntigravityUsageAdapter,
    claude::ClaudeUsageAdapter,
    copilot::CopilotUsageAdapter,
    cursor::CursorUsageAdapter,
    deepseek::DeepSeekUsageAdapter,
    kimi::KimiUsageAdapter,
    openai::WhamUsageAdapter,
    opencode_go::{OpenCodeGoSourceMode, OpenCodeGoUsageAdapter},
    openrouter::OpenRouterUsageAdapter,
    zai::ZaiUsageAdapter,
};
use crate::{
    accounts::{AccountRecord, OPENAI, OPENCODE_GO},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider},
    transport::{TransportError, UsageHttpTransport},
    usage::UsageAdapter,
};
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;

/// Per-account provider options. Only OpenCode Go has more than one source;
/// every other provider always uses its single source with all enrichments.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderRegistryConfig {
    pub opencode_go_source_mode: OpenCodeGoSourceMode,
}

impl ProviderRegistryConfig {
    /// Host policy for one saved account: pick the source that matches the
    /// credential that account actually has. Shared by every host (CLI and
    /// tray) so their source selection cannot drift apart.
    pub fn for_account(account: &AccountRecord, material: Option<&AccountAuthMaterial>) -> Self {
        let mut config = Self::default();
        if account.provider_id == OPENCODE_GO {
            // A console sign-in (device authorization) leaves a refresh token;
            // anything else is an OpenCode API key.
            let has_console_oauth = material.is_some_and(|material| {
                material
                    .oauth_refresh_token
                    .as_deref()
                    .is_some_and(|token| !token.trim().is_empty())
            });
            config.opencode_go_source_mode = if has_console_oauth {
                OpenCodeGoSourceMode::Web
            } else {
                OpenCodeGoSourceMode::Api
            };
        }
        config
    }
}

#[derive(Debug, Error)]
pub enum ProviderRegistryError {
    #[error("provider adapter id is empty")]
    EmptyAdapterId,
    #[error("duplicate provider adapter id or alias: {0}")]
    DuplicateId(String),
    #[error("provider adapter construction failed: {0}")]
    Construction(#[from] TransportError),
}

/// A canonical adapter set plus lookup aliases for persisted account IDs.
pub struct ProviderRegistry {
    adapters: HashMap<String, Arc<dyn UsageAdapter>>,
    canonical_ids: Vec<String>,
}

impl ProviderRegistry {
    /// Builds a registry from already constructed adapters. This is useful for
    /// tests and for hosts that want to inject a custom adapter.
    pub fn from_adapters(
        adapters: impl IntoIterator<Item = Arc<dyn UsageAdapter>>,
    ) -> Result<Self, ProviderRegistryError> {
        let mut lookup = HashMap::new();
        let mut canonical_ids = Vec::new();
        for adapter in adapters {
            let implementation_id = normalize_id(adapter.adapter_id())?;
            let canonical_id = if implementation_id == "openai-wham" {
                OPENAI.to_owned()
            } else {
                implementation_id.clone()
            };
            insert_unique(&mut lookup, implementation_id.clone(), Arc::clone(&adapter))?;
            if canonical_id != implementation_id {
                insert_unique(&mut lookup, canonical_id.clone(), Arc::clone(&adapter))?;
            }
            canonical_ids.push(canonical_id.clone());

            // The OpenAI adapter's implementation id is intentionally more
            // specific than the persisted provider id. Keep both legacy names
            // mapped to the exact same adapter instance.
            if canonical_id == OPENAI {
                insert_unique(&mut lookup, "codex".to_owned(), adapter)?;
            }
        }
        canonical_ids.sort_unstable();
        Ok(Self {
            adapters: lookup,
            canonical_ids,
        })
    }

    /// Constructs the complete first-party adapter set from shared transport
    /// and authentication dependencies. No provider is allowed to create its
    /// own credential store or refresh loop here.
    pub fn from_dependencies(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        config: ProviderRegistryConfig,
    ) -> Result<Self, ProviderRegistryError> {
        Self::from_dependencies_inner(transport, auth, config)
    }

    fn from_dependencies_inner(
        transport: Arc<dyn UsageHttpTransport>,
        auth: Arc<dyn AccountAuthMaterialProvider>,
        config: ProviderRegistryConfig,
    ) -> Result<Self, ProviderRegistryError> {
        let openai = Arc::new(WhamUsageAdapter::new(
            Arc::clone(&transport),
            Arc::clone(&auth),
        )?) as Arc<dyn UsageAdapter>;
        let claude = Arc::new(
            ClaudeUsageAdapter::new(Arc::clone(&transport), Arc::clone(&auth))?
                .with_account_identity(true),
        ) as Arc<dyn UsageAdapter>;
        let opencode_go = Arc::new(
            OpenCodeGoUsageAdapter::new(Arc::clone(&transport), Arc::clone(&auth))?
                .with_source_mode(config.opencode_go_source_mode),
        ) as Arc<dyn UsageAdapter>;
        let openrouter = Arc::new(
            OpenRouterUsageAdapter::new(Arc::clone(&transport), Arc::clone(&auth), true)?
                .with_activity(true),
        ) as Arc<dyn UsageAdapter>;
        let antigravity = Arc::new(AntigravityUsageAdapter::new(
            Arc::clone(&transport),
            Arc::clone(&auth),
        )?) as Arc<dyn UsageAdapter>;

        let deepseek = Arc::new(DeepSeekUsageAdapter::new(
            Arc::clone(&transport),
            Arc::clone(&auth),
        )?) as Arc<dyn UsageAdapter>;

        let copilot = Arc::new(CopilotUsageAdapter::new(
            Arc::clone(&transport),
            Arc::clone(&auth),
        )?) as Arc<dyn UsageAdapter>;

        let cursor = Arc::new(CursorUsageAdapter::new(
            Arc::clone(&transport),
            Arc::clone(&auth),
        )?) as Arc<dyn UsageAdapter>;

        let kimi = Arc::new(KimiUsageAdapter::new(
            Arc::clone(&transport),
            Arc::clone(&auth),
        )?) as Arc<dyn UsageAdapter>;

        let zai = Arc::new(ZaiUsageAdapter::new(
            Arc::clone(&transport),
            Arc::clone(&auth),
        )?) as Arc<dyn UsageAdapter>;

        Self::from_adapters([
            openai,
            claude,
            opencode_go,
            openrouter,
            antigravity,
            deepseek,
            copilot,
            cursor,
            kimi,
            zai,
        ])
    }

    pub fn get(&self, provider_id: &str) -> Option<Arc<dyn UsageAdapter>> {
        self.adapters
            .get(&provider_id.trim().to_ascii_lowercase())
            .cloned()
    }

    pub fn contains(&self, provider_id: &str) -> bool {
        self.get(provider_id).is_some()
    }

    /// Returns canonical adapter instances once each; aliases are omitted.
    pub fn adapters(&self) -> Vec<Arc<dyn UsageAdapter>> {
        self.canonical_ids
            .iter()
            .filter_map(|id| self.adapters.get(id).cloned())
            .collect()
    }

    pub fn canonical_ids(&self) -> &[String] {
        &self.canonical_ids
    }

    pub fn len(&self) -> usize {
        self.canonical_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.canonical_ids.is_empty()
    }
}

fn normalize_id(value: &str) -> Result<String, ProviderRegistryError> {
    let id = value.trim().to_ascii_lowercase();
    if id.is_empty() {
        return Err(ProviderRegistryError::EmptyAdapterId);
    }
    Ok(id)
}

fn insert_unique(
    lookup: &mut HashMap<String, Arc<dyn UsageAdapter>>,
    id: String,
    adapter: Arc<dyn UsageAdapter>,
) -> Result<(), ProviderRegistryError> {
    if lookup.insert(id.clone(), adapter).is_some() {
        return Err(ProviderRegistryError::DuplicateId(id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        accounts::{
            ANTIGRAVITY, AccountRecord, CLAUDE, COPILOT, CURSOR, DEEPSEEK, KIMI, OPENCODE_GO,
            OPENROUTER, ZAI,
        },
        transport::TransportError,
        usage::{UsageAdapter, UsageProbeResult},
    };
    use async_trait::async_trait;

    struct TestAdapter(&'static str);

    #[async_trait]
    impl UsageAdapter for TestAdapter {
        fn adapter_id(&self) -> &str {
            self.0
        }

        async fn probe(
            &self,
            _account: &AccountRecord,
        ) -> Result<UsageProbeResult, TransportError> {
            unreachable!("registry tests never probe")
        }
    }

    #[test]
    fn openai_legacy_aliases_resolve_to_one_canonical_adapter() {
        let registry = ProviderRegistry::from_adapters([
            Arc::new(TestAdapter("openai-wham")) as Arc<dyn UsageAdapter>,
            Arc::new(TestAdapter(CLAUDE)) as Arc<dyn UsageAdapter>,
        ])
        .unwrap();
        assert_eq!(
            registry.canonical_ids(),
            &[CLAUDE.to_owned(), OPENAI.to_owned()]
        );
        assert!(registry.contains(OPENAI));
        assert!(registry.contains("codex"));
        let openai = registry.get(OPENAI).unwrap();
        let codex = registry.get("codex").unwrap();
        assert!(Arc::ptr_eq(&openai, &codex));
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let error = match ProviderRegistry::from_adapters([
            Arc::new(TestAdapter(OPENROUTER)) as Arc<dyn UsageAdapter>,
            Arc::new(TestAdapter(OPENROUTER)) as Arc<dyn UsageAdapter>,
        ]) {
            Ok(_) => panic!("duplicate adapter ids must be rejected"),
            Err(error) => error,
        };
        assert!(matches!(error, ProviderRegistryError::DuplicateId(id) if id == OPENROUTER));
    }

    #[test]
    fn all_first_party_adapters_can_be_constructed_without_network_calls() {
        let transport = Arc::new(
            crate::transport::ReqwestUsageHttpTransport::new(std::time::Duration::from_secs(1))
                .unwrap(),
        ) as Arc<dyn UsageHttpTransport>;
        let auth = Arc::new(crate::auth::EmptyAuthMaterialProvider)
            as Arc<dyn AccountAuthMaterialProvider>;
        let registry =
            ProviderRegistry::from_dependencies(transport, auth, ProviderRegistryConfig::default())
                .unwrap();
        assert_eq!(registry.len(), 10);
        assert!(registry.contains(KIMI));
        assert!(registry.contains(ZAI));
        assert!(registry.contains(COPILOT));
        assert!(registry.contains(CURSOR));
        assert!(registry.contains(DEEPSEEK));
        assert!(registry.contains(ANTIGRAVITY));
        assert!(registry.contains(OPENCODE_GO));
    }

    #[test]
    fn opencode_go_source_follows_the_saved_credential() {
        let opencode =
            AccountRecord::create("go", "go@example.com", None, OPENCODE_GO, None).unwrap();
        let console_oauth = AccountAuthMaterial {
            bearer_token: Some("access".to_owned()),
            oauth_refresh_token: Some("refresh".to_owned()),
            ..AccountAuthMaterial::default()
        };
        assert_eq!(
            ProviderRegistryConfig::for_account(&opencode, Some(&console_oauth))
                .opencode_go_source_mode,
            OpenCodeGoSourceMode::Web
        );
        let api_key = AccountAuthMaterial {
            bearer_token: Some("key".to_owned()),
            ..AccountAuthMaterial::default()
        };
        assert_eq!(
            ProviderRegistryConfig::for_account(&opencode, Some(&api_key)).opencode_go_source_mode,
            OpenCodeGoSourceMode::Api
        );
    }
}
