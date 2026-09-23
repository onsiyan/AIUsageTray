use codex_usage_core::{
    accounts::{ANTIGRAVITY, AccountId, AccountRecord, AccountStore},
    auth::{
        AccountAuthMaterialProvider, AccountOAuthMaterialProvider, CompositeAuthMaterialProvider,
        OAuthCredentialProviderRegistry, OAuthCredentialStore, StoredAuthMaterialProvider,
        StoredOAuthCredential,
    },
    auth_sources::{EnvironmentAuthMaterialProvider, LocalFileAuthMaterialProvider},
    oauth_loopback::LoopbackOAuthCallbackListenerFactory,
    oauth_service::OAuthAuthorizationService,
    providers::antigravity::oauth_definition,
    providers::registry::ProviderRegistryConfig,
    refresh::{RefreshCoordinatorConfig, RefreshReason, RefreshStatus},
    runtime::UsageRuntime,
    storage::{SqliteStore, default_accounts_database_path},
    transport::{ReqwestUsageHttpTransport, UsageHttpTransport},
};
use codex_usage_windows_auth::{
    WindowsCredentialManagerAuthMaterialStore, WindowsCredentialManagerStore,
    WindowsDefaultBrowserLauncher,
};
use std::{sync::Arc, time::Duration};

type Transport = ReqwestUsageHttpTransport;
type CredentialStore = WindowsCredentialManagerStore;
type CallbackFactory = LoopbackOAuthCallbackListenerFactory;
type Browser = WindowsDefaultBrowserLauncher;
type Authorization =
    OAuthAuthorizationService<Transport, CredentialStore, CallbackFactory, Browser>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let force_new_account =
        std::env::args().any(|argument| matches!(argument.as_str(), "--new" | "--login-new"));
    let database_path = default_accounts_database_path();
    let account_store = Arc::new(SqliteStore::open(&database_path)?);
    let transport = Arc::new(ReqwestUsageHttpTransport::new(Duration::from_secs(45))?);
    let credential_store = Arc::new(WindowsCredentialManagerStore);
    let provider = oauth_definition();

    let account = if let Some(account) = if !force_new_account {
        account_store
            .list()
            .await?
            .into_iter()
            .find(|account| account.provider_id == ANTIGRAVITY)
    } else {
        None
    } {
        println!("Reusing the Rust-isolated Antigravity account.");
        account
    } else {
        if force_new_account {
            println!("Opening a new isolated Antigravity account login.");
        }
        println!("The default browser will open for the Rust OAuth test.");
        println!("Complete login and consent, then return to this terminal.");
        let authorization = Arc::new(create_authorization(
            transport.clone(),
            credential_store.clone(),
        ));
        let pending = AccountRecord::create(
            "Antigravity account",
            "pending@local.invalid",
            None,
            ANTIGRAVITY,
            None,
        )?;
        let login = authorization
            .login(pending.id, &provider, Duration::from_secs(300))
            .await?;
        let identity = login.identity.as_ref();
        let account = pending.with_identity(
            identity.and_then(|identity| identity.email.as_deref()),
            identity.and_then(|identity| identity.provider_account_id.as_deref()),
        )?;
        let account = persist_oauth_login_account(
            account_store.as_ref(),
            credential_store.as_ref(),
            account,
            pending.id,
            &login.credential,
        )
        .await?;
        println!("Rust OAuth login succeeded.");
        account
    };

    probe_and_print(
        &database_path,
        transport.clone(),
        credential_store.clone(),
        &provider,
        &account,
    )
    .await?;

    println!("Restarting Rust authorization service to verify Credential Manager reload...");
    probe_and_print(
        &database_path,
        transport,
        credential_store,
        &provider,
        &account,
    )
    .await?;

    println!("Rust OAuth probe completed successfully.");
    println!("Rust-local database: {}", database_path.display());
    Ok(())
}

async fn persist_oauth_login_account(
    account_store: &dyn AccountStore,
    credential_store: &dyn OAuthCredentialStore,
    account: AccountRecord,
    provisional_account_id: AccountId,
    credential: &StoredOAuthCredential,
) -> Result<AccountRecord, Box<dyn std::error::Error>> {
    let resolved = account_store
        .upsert_or_get_by_provider_identity(&account)
        .await?;
    if resolved.id == provisional_account_id {
        return Ok(resolved);
    }

    if let Err(error) = credential_store.save(resolved.id, credential).await {
        if let Err(cleanup_error) = credential_store.remove(provisional_account_id).await {
            eprintln!(
                "Warning: temporary OAuth credential cleanup failed after account reuse: {cleanup_error}"
            );
        }
        return Err(error.into());
    }

    if let Err(error) = credential_store.remove(provisional_account_id).await {
        eprintln!(
            "Warning: the existing account was reused, but its temporary OAuth credential could not be removed: {error}"
        );
    }
    println!(
        "This Antigravity identity is already linked; reused its account and refreshed its saved OAuth credential."
    );
    Ok(resolved)
}

fn create_authorization(
    transport: Arc<Transport>,
    credential_store: Arc<CredentialStore>,
) -> Authorization {
    OAuthAuthorizationService::new(
        transport,
        credential_store,
        Arc::new(LoopbackOAuthCallbackListenerFactory),
        Arc::new(WindowsDefaultBrowserLauncher),
    )
}

#[cfg(test)]
mod tests {
    use super::persist_oauth_login_account;
    use codex_usage_core::{
        accounts::{ANTIGRAVITY, AccountRecord, AccountStore, InMemoryAccountStore},
        auth::{InMemoryOAuthCredentialStore, OAuthCredentialStore, StoredOAuthCredential},
    };
    use std::{collections::BTreeMap, sync::Arc};

    fn credential(refresh_token: &str, provider_account_id: &str) -> StoredOAuthCredential {
        StoredOAuthCredential {
            provider_id: ANTIGRAVITY.to_owned(),
            refresh_token: refresh_token.to_owned(),
            client_id: Some("antigravity-client".to_owned()),
            client_secret: None,
            id_token: None,
            provider_account_id: Some(provider_account_id.to_owned()),
            workspace_id: None,
            metadata: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn adding_the_same_provider_identity_reuses_account_and_moves_credential() {
        let accounts = Arc::new(InMemoryAccountStore::default());
        let credentials = Arc::new(InMemoryOAuthCredentialStore::default());
        let identity = "google-subject-1";
        let existing = AccountRecord::create(
            "Antigravity main",
            "user@example.com",
            Some(identity.to_owned()),
            ANTIGRAVITY,
            None,
        )
        .unwrap();
        accounts.upsert(&existing).await.unwrap();
        credentials
            .save(existing.id, &credential("old-refresh-token", identity))
            .await
            .unwrap();

        let provisional = AccountRecord::create(
            "Antigravity account",
            "user@example.com",
            Some(identity.to_owned()),
            ANTIGRAVITY,
            None,
        )
        .unwrap();
        credentials
            .save(provisional.id, &credential("new-refresh-token", identity))
            .await
            .unwrap();

        let resolved = persist_oauth_login_account(
            accounts.as_ref(),
            credentials.as_ref(),
            provisional.clone(),
            provisional.id,
            &credential("new-refresh-token", identity),
        )
        .await
        .unwrap();

        assert_eq!(resolved.id, existing.id);
        assert_eq!(accounts.list().await.unwrap().len(), 1);
        assert_eq!(
            credentials
                .get(existing.id)
                .await
                .unwrap()
                .unwrap()
                .refresh_token,
            "new-refresh-token"
        );
        assert!(credentials.get(provisional.id).await.unwrap().is_none());
    }
}

async fn probe_and_print(
    database_path: &std::path::Path,
    transport: Arc<Transport>,
    credential_store: Arc<CredentialStore>,
    provider: &codex_usage_core::auth::OAuthProviderDefinition,
    account: &AccountRecord,
) -> Result<(), Box<dyn std::error::Error>> {
    let authorization = Arc::new(create_authorization(
        transport.clone(),
        credential_store.clone(),
    ));
    let registry = Arc::new(OAuthCredentialProviderRegistry::new([provider.clone()]));
    let oauth_material = Arc::new(AccountOAuthMaterialProvider {
        authorization,
        credentials: credential_store,
        providers: registry,
    }) as Arc<dyn AccountAuthMaterialProvider>;
    let secure_material_store = Arc::new(WindowsCredentialManagerAuthMaterialStore);
    let imported_material = Arc::new(StoredAuthMaterialProvider::new(
        secure_material_store.clone(),
    )) as Arc<dyn AccountAuthMaterialProvider>;
    let local_file_material = Arc::new(LocalFileAuthMaterialProvider::from_process(account.id))
        as Arc<dyn AccountAuthMaterialProvider>;
    let environment_material = Arc::new(EnvironmentAuthMaterialProvider::from_process(account.id))
        as Arc<dyn AccountAuthMaterialProvider>;
    let base_auth_material = Arc::new(CompositeAuthMaterialProvider::new([
        imported_material,
        oauth_material,
        local_file_material,
        environment_material,
    ])) as Arc<dyn AccountAuthMaterialProvider>;
    let runtime = UsageRuntime::from_sqlite_path_with_auth_store(
        database_path,
        transport as Arc<dyn UsageHttpTransport>,
        base_auth_material,
        secure_material_store,
        ProviderRegistryConfig::default(),
        RefreshCoordinatorConfig {
            cadence: codex_usage_core::refresh::RefreshCadence::Manual,
            ..RefreshCoordinatorConfig::default()
        },
    )?;
    let outcome = runtime
        .refresh_account(account.clone(), RefreshReason::Manual)
        .await;
    if outcome.status != RefreshStatus::Updated {
        let message = outcome
            .error
            .as_ref()
            .map(|error| error.message.clone())
            .unwrap_or_else(|| "usage refresh did not produce a snapshot".to_owned());
        return Err(message.into());
    }
    let snapshot = outcome.snapshot.expect("updated outcome has a snapshot");
    println!(
        "Plan: {}",
        snapshot.plan_type.as_deref().unwrap_or("unknown")
    );
    println!(
        "Source: {} | quota windows: {} | metrics/models: {}",
        snapshot.source.as_deref().unwrap_or("unknown"),
        snapshot.additional_windows.len(),
        snapshot.metrics.len()
    );
    for metric in &snapshot.metrics {
        let remaining = metric
            .remaining_percent()
            .map(|value| format!("{value:.2}% remaining"))
            .or_else(|| {
                metric
                    .remaining_amount
                    .map(|value| format!("{value:.2}% remaining"))
            })
            .unwrap_or_else(|| "unknown remaining quota".to_owned());
        println!("{}: {remaining}", metric.name);
    }
    Ok(())
}
