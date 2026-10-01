//! Account-bound credentials read from the environment.
//!
//! OpenRouter accounts are added with an API key passed through the
//! environment (or stdin). The variables are bound to one account so a global
//! `OPENROUTER_API_KEY` is never silently applied to every OpenRouter account.

use crate::{
    accounts::{AccountId, AccountRecord, OPENROUTER},
    auth::{AccountAuthMaterial, AccountAuthMaterialProvider, AuthError},
};
use async_trait::async_trait;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct EnvironmentAuthMaterialProvider {
    account_id: AccountId,
    environment: HashMap<String, String>,
}

impl EnvironmentAuthMaterialProvider {
    pub fn with_environment(
        account_id: AccountId,
        environment: impl IntoIterator<Item = (String, String)>,
    ) -> Self {
        Self {
            account_id,
            environment: environment.into_iter().collect(),
        }
    }

    /// A trimmed value with one pair of surrounding quotes removed.
    fn value(&self, key: &str) -> Option<String> {
        let value = self.environment.get(key)?.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            value[1..value.len() - 1].trim()
        } else {
            value
        };
        (!value.is_empty()).then(|| value.to_owned())
    }
}

#[async_trait]
impl AccountAuthMaterialProvider for EnvironmentAuthMaterialProvider {
    async fn get(&self, account: &AccountRecord) -> Result<Option<AccountAuthMaterial>, AuthError> {
        if account.id != self.account_id || account.provider_id != OPENROUTER {
            return Ok(None);
        }
        let material = AccountAuthMaterial {
            bearer_token: self.value("OPENROUTER_API_KEY"),
            secondary_bearer_token: self.value("OPENROUTER_MANAGEMENT_API_KEY"),
            ..AccountAuthMaterial::default()
        };
        Ok((!material.is_empty()).then_some(material))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{
        AccountAuthMaterialStore, CompositeAuthMaterialProvider, InMemoryAuthMaterialStore,
        StoredAuthMaterialProvider,
    };
    use std::sync::Arc;

    fn account() -> AccountRecord {
        AccountRecord::create("test", "user@example.com", None, OPENROUTER, None).unwrap()
    }

    #[tokio::test]
    async fn environment_source_is_scoped_to_one_account_and_maps_management_key() {
        let primary = account();
        let other = account();
        let source = EnvironmentAuthMaterialProvider::with_environment(
            primary.id,
            [
                ("OPENROUTER_API_KEY".to_owned(), "\"sk-primary\"".to_owned()),
                (
                    "OPENROUTER_MANAGEMENT_API_KEY".to_owned(),
                    "sk-management".to_owned(),
                ),
            ],
        );

        let material = source.get(&primary).await.unwrap().unwrap();
        assert_eq!(material.bearer_token.as_deref(), Some("sk-primary"));
        assert_eq!(
            material.secondary_bearer_token.as_deref(),
            Some("sk-management")
        );
        assert!(source.get(&other).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn stored_material_and_environment_merge_without_cross_account_leak() {
        let selected = account();
        let other = account();
        let store = Arc::new(InMemoryAuthMaterialStore::default());
        store
            .save(
                selected.id,
                &AccountAuthMaterial {
                    secondary_bearer_token: Some("stored-management".to_owned()),
                    ..AccountAuthMaterial::default()
                },
            )
            .await
            .unwrap();
        let stored = Arc::new(StoredAuthMaterialProvider::new(store))
            as Arc<dyn AccountAuthMaterialProvider>;
        let environment = Arc::new(EnvironmentAuthMaterialProvider::with_environment(
            selected.id,
            [("OPENROUTER_API_KEY".to_owned(), "api-key".to_owned())],
        )) as Arc<dyn AccountAuthMaterialProvider>;
        let composite = CompositeAuthMaterialProvider::new([stored, environment]);

        let material = composite.get(&selected).await.unwrap().unwrap();
        assert_eq!(material.bearer_token.as_deref(), Some("api-key"));
        assert_eq!(
            material.secondary_bearer_token.as_deref(),
            Some("stored-management")
        );
        assert!(composite.get(&other).await.unwrap().is_none());
    }
}
