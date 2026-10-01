//! Opening the database and selecting an account by reference, alias, name, or email.

use super::*;

pub(super) fn open_store(path: &std::path::Path) -> Result<Arc<SqliteStore>, CliFailure> {
    SqliteStore::open(path)
        .map(Arc::new)
        .map_err(|error| CliFailure::runtime(error.to_string()))
}

pub(super) fn resolve_account<'a>(
    accounts: &'a [AccountRecord],
    selector: &str,
    filters: &SelectorFilters<'_>,
) -> Result<&'a AccountRecord, CliFailure> {
    let selector = selector.trim();
    if let Some(account) = accounts.iter().find(|account| {
        account
            .account_ref
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(selector))
    }) {
        let provider_matches = filters
            .provider
            .is_none_or(|provider| provider_matches(&account.provider_id, provider));
        let workspace_matches = filters
            .workspace
            .is_none_or(|workspace| workspace_matches(account, workspace));
        return if provider_matches && workspace_matches {
            Ok(account)
        } else {
            Err(CliFailure::new(
                "account_not_found",
                format!("Account reference `{selector}` does not match the supplied filters."),
                3,
            ))
        };
    }

    let matches = accounts
        .iter()
        .filter(|account| {
            filters
                .provider
                .is_none_or(|provider| provider_matches(&account.provider_id, provider))
        })
        .filter(|account| {
            filters
                .workspace
                .is_none_or(|workspace| workspace_matches(account, workspace))
        })
        .filter(|account| {
            account
                .alias
                .as_deref()
                .is_some_and(|value| value.eq_ignore_ascii_case(selector))
                || account.label.eq_ignore_ascii_case(selector)
                || account.email.eq_ignore_ascii_case(selector)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [account] => Ok(*account),
        [] => Err(CliFailure::new(
            "account_not_found",
            format!("No saved account matches `{selector}` and the supplied filters."),
            3,
        )),
        _ => Err(ambiguous_account_failure(selector, &matches)),
    }
}

pub(super) fn ambiguous_account_failure(selector: &str, matches: &[&AccountRecord]) -> CliFailure {
    let candidates = matches
        .iter()
        .filter_map(|account| account.account_ref.clone())
        .collect::<Vec<_>>();
    let mut failure = CliFailure::new(
        "ambiguous_account",
        format!("`{selector}` matches more than one account; add --provider or --workspace."),
        4,
    );
    failure.candidates = candidates;
    failure
}

pub(super) fn provider_matches(account_provider: &str, requested: &str) -> bool {
    let account_provider = normalize_name(account_provider);
    let requested = normalize_name(requested);
    if requested == "codex" {
        return account_provider == normalize_name(OPENAI) || account_provider == "codex";
    }
    account_provider == requested
}

pub(super) fn normalize_name(value: &str) -> String {
    value
        .trim()
        .chars()
        .filter(|character| *character != '-' && *character != '_')
        .flat_map(char::to_lowercase)
        .collect()
}

pub(super) fn workspace_matches(account: &AccountRecord, requested: &str) -> bool {
    account
        .workspace_id
        .as_deref()
        .is_some_and(|value| value.eq_ignore_ascii_case(requested.trim()))
        || account
            .workspace_name
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case(requested.trim()))
}
