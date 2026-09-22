//! Construction and lookup of the provider adapters used by the runtime.
//!
//! The registry is the only place that knows which concrete adapters are
//! enabled. The refresh coordinator receives the registry's trait objects and
//! remains unaware of provider constructors and provider-specific options.

use super::{
    antigravity::AntigravityUsageAdapter,
    claude::ClaudeSourceMode,
    claude::ClaudeUsageAdapter,
    claude_planner::ClaudeRuntime,
    openai::WhamUsageAdapter,
    opencode_go::{OpenCodeGoSourceMode, OpenCodeGoUsageAdapter},
    openrouter::OpenRouterUsageAdapter,
};
use crate::{
    accounts::OPENAI,
    auth::AccountAuthMaterialProvider,
    transport::{TransportError, UsageHttpTransport},
    usage::UsageAdapter,
};
use std::{collections::HashMap, sync::Arc};
use thiserror::Error;

/// Options that affect provider-side enrichment requests. Usage extraction
/// remains enabled for every provider in this configuration.
#[derive(Debug, Clone, Copy)]
pub struct ProviderRegistryConfig {
    pub fetch_openai_spend_controls: bool,
    pub fetch_openai_workspace_balance: bool,
    pub fetch_openai_reset_credits: bool,
    pub fetch_claude_prepaid_credits: bool,
    /// Optional `/api/oauth/profile` and `/api/account` enrichment. The
    /// usage routes remain authoritative even when these identity calls fail.
    pub fetch_claude_account_identity: bool,
    /// Optional browser-session enrichment after an OAuth/CLI usage probe.
    /// This is separate from prepaid-credit requests because the extra usage
    /// windows are useful even when credit balances are disabled.
    pub fetch_claude_web_extras: bool,
    pub claude_source_mode: ClaudeSourceMode,
    pub claude_runtime: ClaudeRuntime,
    pub opencode_go_source_mode: OpenCodeGoSourceMode,
    pub fetch_openrouter_credits: bool,
    /// Optional 30-day Activity enrichment. The key quota remains enabled
    /// even when this management-key-only request is disabled.
    pub fetch_openrouter_activity: bool,
    pub enable_antigravity_local_probe: bool,
}

impl Default for ProviderRegistryConfig {
    fn default() -> Self {
        Self {
            fetch_openai_spend_controls: false,
            fetch_openai_workspace_balance: false,
            fetch_openai_reset_credits: true,
            fetch_claude_prepaid_credits: true,
            fetch_claude_account_identity: true,
            fetch_claude_web_extras: true,
            claude_source_mode: ClaudeSourceMode::Automatic,
            claude_runtime: ClaudeRuntime::App,
            opencode_go_source_mode: OpenCodeGoSourceMode::Automatic,
            fetch_openrouter_credits: true,
            fetch_openrouter_activity: true,
            enable_antigravity_local_probe: true,
        }
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
        let openai_adapter = WhamUsageAdapter::new(
            Arc::clone(&transport),
            Arc::clone(&auth),
            config.fetch_openai_spend_controls,
            config.fetch_openai_workspace_balance,
        )?
        .with_reset_credits(config.fetch_openai_reset_credits);
        let openai = Arc::new(openai_adapter) as Arc<dyn UsageAdapter>;
        let claude = Arc::new(
            ClaudeUsageAdapter::new(
                Arc::clone(&transport),
                Arc::clone(&auth),
                config.fetch_claude_prepaid_credits,
            )?
            .with_account_identity(config.fetch_claude_account_identity)
            .with_web_extras(config.fetch_claude_web_extras)
            .with_source_mode(config.claude_source_mode)
            .with_runtime(config.claude_runtime),
        ) as Arc<dyn UsageAdapter>;
        let opencode_go = Arc::new(
            OpenCodeGoUsageAdapter::new(Arc::clone(&transport), Arc::clone(&auth))?
                .with_source_mode(config.opencode_go_source_mode),
        ) as Arc<dyn UsageAdapter>;
        let openrouter = Arc::new(
            OpenRouterUsageAdapter::new(
                Arc::clone(&transport),
                Arc::clone(&auth),
                config.fetch_openrouter_credits,
            )?
            .with_activity(config.fetch_openrouter_activity),
        ) as Arc<dyn UsageAdapter>;
        let antigravity = if config.enable_antigravity_local_probe {
            Arc::new(AntigravityUsageAdapter::new(
                Arc::clone(&transport),
                Arc::clone(&auth),
            )?) as Arc<dyn UsageAdapter>
        } else {
            Arc::new(AntigravityUsageAdapter::new_without_local_probe(
                Arc::clone(&transport),
                Arc::clone(&auth),
            )?) as Arc<dyn UsageAdapter>
        };

        Self::from_adapters([openai, claude, opencode_go, openrouter, antigravity])
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
        accounts::{ANTIGRAVITY, AccountRecord, CLAUDE, OPENCODE_GO, OPENROUTER},
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
        let registry = ProviderRegistry::from_dependencies(
            transport,
            auth,
            ProviderRegistryConfig {
                enable_antigravity_local_probe: false,
                ..ProviderRegistryConfig::default()
            },
        )
        .unwrap();
        assert_eq!(registry.len(), 5);
        assert!(registry.contains(ANTIGRAVITY));
        assert!(registry.contains(OPENCODE_GO));
    }
}
