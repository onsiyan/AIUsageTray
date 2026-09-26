use chrono::Utc;
use codex_usage_core::{
    accounts::{AccountRecord, AccountStore, OPENAI},
    storage::SqliteStore,
    usage::{CodexWeeklyResetCandidate, UsagePrimaryWindowKind, UsageSnapshot, UsageSnapshotStore},
};
use tempfile::tempdir;

fn candidate(account_id: codex_usage_core::accounts::AccountId) -> CodexWeeklyResetCandidate {
    let observed_at_utc = Utc::now();
    CodexWeeklyResetCandidate {
        evidence_version: 1,
        first_observed_at_utc: observed_at_utc,
        created_at_utc: observed_at_utc,
        snapshot: UsageSnapshot {
            account_id,
            observed_at_utc,
            response_account_id: Some("provider-account".to_owned()),
            plan_type: Some("team".to_owned()),
            primary: None,
            primary_window_kind: Some(UsagePrimaryWindowKind::Weekly),
            primary_window_is_synthetic: false,
            secondary: None,
            additional_windows: Vec::new(),
            credits: None,
            credit_inventory: None,
            spend: None,
            observed_email: Some("codex@example.com".to_owned()),
            is_stale: false,
            stale_reason: None,
            stale_at_utc: None,
            metrics: Vec::new(),
            source_diagnostics: Vec::new(),
            provider_id: OPENAI.to_owned(),
            source: Some("codex-oauth".to_owned()),
            data_confidence: "authoritative".to_owned(),
        },
    }
}

#[tokio::test]
async fn codex_weekly_reset_candidate_survives_reopen_and_can_be_cleared() {
    let directory = tempdir().unwrap();
    let database_path = directory.path().join("accounts.db");
    let account = AccountRecord::create(
        "Codex test",
        "codex@example.com",
        Some("provider-account".to_owned()),
        OPENAI,
        None,
    )
    .unwrap();
    let expected = candidate(account.id);

    {
        let store = SqliteStore::open(&database_path).unwrap();
        store.upsert(&account).await.unwrap();
        store
            .save_codex_weekly_reset_candidate(account.id, Some(expected.clone()))
            .await
            .unwrap();
    }

    let reopened = SqliteStore::open(&database_path).unwrap();
    let actual = reopened
        .get_codex_weekly_reset_candidate(account.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.evidence_version, expected.evidence_version);
    assert_eq!(actual.first_observed_at_utc, expected.first_observed_at_utc);
    assert_eq!(actual.created_at_utc, expected.created_at_utc);
    assert_eq!(actual.snapshot.account_id, expected.snapshot.account_id);
    assert_eq!(actual.snapshot.source, expected.snapshot.source);

    reopened
        .save_codex_weekly_reset_candidate(account.id, None)
        .await
        .unwrap();
    assert!(
        reopened
            .get_codex_weekly_reset_candidate(account.id)
            .await
            .unwrap()
            .is_none()
    );
}
