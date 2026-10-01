//! `account` commands: list, get, alias, and remove.

use super::*;

pub(super) async fn execute_account_command(
    database_path: &std::path::Path,
    command: AccountCommand,
    json_output: bool,
) -> Result<i32, CliFailure> {
    let store = open_store(database_path)?;
    let account_store: Arc<dyn AccountStore> = store.clone();
    match command {
        AccountCommand::Add(arguments) => {
            execute_account_add(database_path, &account_store, arguments, json_output).await
        }
        AccountCommand::List {
            provider,
            workspace,
        } => {
            let accounts = account_store
                .list()
                .await
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            let accounts = accounts
                .iter()
                .filter(|account| {
                    provider
                        .as_deref()
                        .is_none_or(|value| provider_matches(&account.provider_id, value))
                })
                .filter(|account| {
                    workspace
                        .as_deref()
                        .is_none_or(|value| workspace_matches(account, value))
                })
                .collect::<Vec<_>>();
            if json_output {
                print_json(json!({
                    "schema_version": 1,
                    "accounts": accounts.iter().map(|account| account_value(account)).collect::<Vec<_>>(),
                }));
            } else {
                print_account_table(&accounts);
            }
            Ok(0)
        }
        AccountCommand::Get(arguments) => {
            let accounts = account_store
                .list()
                .await
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            let account = resolve_account(
                &accounts,
                &arguments.selector,
                &SelectorFilters {
                    provider: arguments.provider.as_deref(),
                    workspace: arguments.workspace.as_deref(),
                },
            )?;
            if json_output {
                print_json(json!({
                    "schema_version": 1,
                    "account": account_value(account),
                }));
            } else {
                print_account_details(account);
            }
            Ok(0)
        }
        AccountCommand::Remove(arguments) => {
            execute_account_remove(&account_store, arguments, json_output).await
        }
        AccountCommand::Alias { command } => {
            let (selector, alias, provider, workspace) = match command {
                AliasCommand::Set {
                    selector,
                    alias,
                    provider,
                    workspace,
                } => (selector, Some(alias), provider, workspace),
                AliasCommand::Clear {
                    selector,
                    provider,
                    workspace,
                } => (selector, None, provider, workspace),
            };
            let accounts = account_store
                .list()
                .await
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            let account = resolve_account(
                &accounts,
                &selector,
                &SelectorFilters {
                    provider: provider.as_deref(),
                    workspace: workspace.as_deref(),
                },
            )?;
            let updated = account_store
                .set_alias(account.id, alias.as_deref())
                .await
                .map_err(|error| CliFailure::runtime(error.to_string()))?
                .ok_or_else(|| {
                    CliFailure::new(
                        "account_not_found",
                        "The account disappeared during alias update.",
                        3,
                    )
                })?;
            if json_output {
                print_json(json!({
                    "schema_version": 1,
                    "account": account_value(&updated),
                }));
            } else {
                print_account_details(&updated);
            }
            Ok(0)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemovalConfirmationMode {
    Prompt,
    Confirmed,
}

pub(super) fn removal_confirmation_mode(
    yes: bool,
    json_output: bool,
    interactive: bool,
) -> Result<RemovalConfirmationMode, CliFailure> {
    if yes {
        return Ok(RemovalConfirmationMode::Confirmed);
    }
    if json_output || !interactive {
        return Err(CliFailure::new(
            "confirmation_required",
            "Account removal needs confirmation. Re-run with --yes to confirm explicitly.",
            EXIT_OPERATION_NOT_CONFIRMED,
        ));
    }
    Ok(RemovalConfirmationMode::Prompt)
}

pub(super) fn confirm_account_removal(
    account: &AccountRecord,
    yes: bool,
    json_output: bool,
) -> Result<(), CliFailure> {
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    match removal_confirmation_mode(yes, json_output, interactive)? {
        RemovalConfirmationMode::Confirmed => Ok(()),
        RemovalConfirmationMode::Prompt => {
            eprint!(
                "Remove local account {} ({}, {}) and its saved credentials and usage history? This does not revoke access with the provider. [y/N] ",
                account.account_ref.as_deref().unwrap_or("?"),
                provider_name(&account.provider_id),
                account.email,
            );
            std::io::stderr()
                .flush()
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(|error| CliFailure::runtime(error.to_string()))?;
            if matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                Ok(())
            } else {
                Err(CliFailure::new(
                    "operation_cancelled",
                    "Account removal cancelled; no changes were made.",
                    EXIT_OPERATION_NOT_CONFIRMED,
                ))
            }
        }
    }
}

pub(super) async fn execute_account_remove(
    account_store: &Arc<dyn AccountStore>,
    arguments: AccountRemoveArgs,
    json_output: bool,
) -> Result<i32, CliFailure> {
    let accounts = account_store
        .list()
        .await
        .map_err(|error| CliFailure::runtime(error.to_string()))?;
    let account = resolve_account(
        &accounts,
        &arguments.selection.selector,
        &SelectorFilters {
            provider: arguments.selection.provider.as_deref(),
            workspace: arguments.selection.workspace.as_deref(),
        },
    )?
    .clone();

    confirm_account_removal(&account, arguments.yes, json_output)?;

    let oauth_store = WindowsCredentialManagerStore;
    let auth_store = WindowsCredentialManagerAuthMaterialStore;
    remove_local_account_data(&account, account_store.as_ref(), &oauth_store, &auth_store).await?;

    if json_output {
        print_json(json!({
            "schema_version": 1,
            "removed": account_value(&account),
            "removed_local_usage_history": true,
            "provider_access_revoked": false,
        }));
    } else {
        println!(
            "Removed local account {} ({}). Its saved credentials and usage history were deleted; provider access was not revoked.",
            account.account_ref.as_deref().unwrap_or("?"),
            account.email,
        );
    }
    Ok(0)
}

pub(super) async fn remove_local_account_data(
    account: &AccountRecord,
    account_store: &dyn AccountStore,
    oauth_store: &dyn OAuthCredentialStore,
    auth_store: &dyn AccountAuthMaterialStore,
) -> Result<(), CliFailure> {
    if let Err(error) = oauth_store.remove(account.id).await {
        return Err(CliFailure::new(
            "account_removal_failed",
            format!(
                "Could not remove the saved OAuth credential: {error}. The account record and usage history were left in place; credential state may be partial."
            ),
            1,
        ));
    }
    if let Err(error) = auth_store.remove(account.id).await {
        return Err(CliFailure::new(
            "account_removal_partial",
            format!(
                "The OAuth credential was removed, but saved authentication material could not be removed: {error}. The account record and usage history remain."
            ),
            1,
        ));
    }
    if let Err(error) = account_store.remove(account.id).await {
        return Err(CliFailure::new(
            "account_removal_partial",
            format!(
                "Saved credentials were removed, but the account record and usage history could not be removed: {error}."
            ),
            1,
        ));
    }
    Ok(())
}
