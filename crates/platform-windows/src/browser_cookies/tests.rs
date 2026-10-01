
use super::*;
use rusqlite::params;
use tempfile::tempdir;
use usage_monitor_core::{
    accounts::{AccountRecord, CLAUDE},
    auth::{AccountAuthMaterialStore, InMemoryAuthMaterialStore},
};

fn request(domains: &[&str], required: &[&str]) -> BrowserCookieImportRequest {
    BrowserCookieImportRequest {
        browser: None,
        profile_id: None,
        domains: domains.iter().map(|value| (*value).to_owned()).collect(),
        required_cookie_names: required.iter().map(|value| (*value).to_owned()).collect(),
    }
}

fn raw(host: &str, name: &str, value: &str, path: &str) -> RawCookieRow {
    RawCookieRow {
        host: host.to_owned(),
        name: name.to_owned(),
        value: Some(value.to_owned()),
        encrypted_value: Vec::new(),
        path: path.to_owned(),
        expires_utc: 0,
        last_access_utc: 1,
    }
}

#[test]
fn provider_contracts_match_browser_session_cookie_names() {
    let claude = BrowserCookieImportRequest::for_provider(CLAUDE).unwrap();
    assert_eq!(claude.domains, ["claude.ai"]);
    assert_eq!(claude.required_cookie_names, ["sessionKey"]);

    let opencode = BrowserCookieImportRequest::for_provider(OPENCODE_GO).unwrap();
    assert_eq!(
        opencode.required_cookie_names,
        ["auth", "__Host-auth", "__Host-console_session"]
    );
}

#[test]
fn cookie_selection_filters_domains_and_required_names() {
    let rows = vec![
        raw(".claude.ai", "sessionKey", "session-value", "/"),
        raw("evil.example", "sessionKey", "wrong", "/"),
        raw("claude.ai", "other", "ignored", "/"),
    ];
    let selected = select_cookies(rows, &request(&["claude.ai"], &["sessionKey"]), None).unwrap();
    assert_eq!(
        selected.get("sessionkey").map(|value| value.1.as_str()),
        Some("session-value")
    );
    assert_eq!(selected.len(), 1);
}

#[test]
fn exact_host_and_longer_path_win_duplicate_cookie_names() {
    let rows = vec![
        raw(".claude.ai", "sessionKey", "broad", "/"),
        raw("claude.ai", "sessionKey", "exact", "/"),
        raw("claude.ai", "sessionKey", "longer-path", "/settings"),
    ];
    let selected = select_cookies(rows, &request(&["claude.ai"], &["sessionKey"]), None).unwrap();
    assert_eq!(
        selected.get("sessionkey").map(|value| value.1.as_str()),
        Some("longer-path")
    );
}

#[test]
fn expired_cookies_are_not_imported() {
    let mut expired = raw("claude.ai", "sessionKey", "expired", "/");
    expired.expires_utc = chrome_now_micros() - 1;
    let selected = select_cookies(vec![expired], &request(&["claude.ai"], &[]), None).unwrap();
    assert!(selected.is_empty());
}

#[test]
fn app_bound_encryption_is_explicitly_rejected() {
    let error = decode_cookie_value(None, b"v20-app-bound", None).unwrap_err();
    assert!(matches!(
        error,
        BrowserCookieError::UnsupportedEncryption(_)
    ));
}

#[test]
fn import_reads_a_copied_sqlite_profile_without_modifying_source() {
    let root = tempdir().unwrap();
    let browser_root = root.path().join("Google").join("Chrome").join("User Data");
    let profile = browser_root.join("Default").join("Network");
    fs::create_dir_all(&profile).unwrap();
    fs::write(
        browser_root.join("Local State"),
        r#"{"profile":{"info_cache":{"Default":{"name":"Personal"}}}}"#,
    )
    .unwrap();
    let database = profile.join("Cookies");
    let connection = Connection::open(&database).unwrap();
    connection
            .execute_batch(
                "CREATE TABLE cookies (host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB, path TEXT, expires_utc INTEGER, last_access_utc INTEGER);",
            )
            .unwrap();
    connection
        .execute(
            "INSERT INTO cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                ".claude.ai",
                "sessionKey",
                "safe",
                Vec::<u8>::new(),
                "/",
                0,
                1
            ],
        )
        .unwrap();
    drop(connection);

    let importer = WindowsBrowserCookieImporter::with_local_app_data(root.path());
    let mut request = BrowserCookieImportRequest::for_provider(CLAUDE).unwrap();
    request.browser = Some(BrowserKind::Chrome);
    request.profile_id = Some("Default".to_owned());
    let result = importer.import(&request).unwrap();
    assert_eq!(result.cookie_header, "sessionKey=safe");
    assert_eq!(result.display_name.as_deref(), Some("Personal"));
    assert!(database.is_file());
}

#[tokio::test]
async fn import_and_store_is_account_scoped() {
    let root = tempdir().unwrap();
    let browser_root = root.path().join("Google").join("Chrome").join("User Data");
    let profile = browser_root.join("Default").join("Network");
    fs::create_dir_all(&profile).unwrap();
    let database = profile.join("Cookies");
    let connection = Connection::open(&database).unwrap();
    connection
            .execute_batch(
                "CREATE TABLE cookies (host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB, path TEXT, expires_utc INTEGER, last_access_utc INTEGER);",
            )
            .unwrap();
    connection
        .execute(
            "INSERT INTO cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                ".claude.ai",
                "sessionKey",
                "session-value",
                Vec::<u8>::new(),
                "/",
                0,
                1
            ],
        )
        .unwrap();
    drop(connection);

    let importer = WindowsBrowserCookieImporter::with_local_app_data(root.path());
    let mut account =
        AccountRecord::create("Claude", "user@example.com", None, CLAUDE, None).unwrap();
    account.browser_profile_id = Some("Default".to_owned());
    let store = InMemoryAuthMaterialStore::default();
    let imported =
        import_and_store_for_account(&importer, &store, &account, Some(BrowserKind::Chrome))
            .await
            .unwrap();
    assert_eq!(imported.cookie_header, "sessionKey=session-value");
    let stored = store.get(account.id).await.unwrap().unwrap();
    assert_eq!(
        stored.cookie_header().as_deref(),
        Some("sessionKey=session-value")
    );
}

#[tokio::test]
async fn session_reimport_uses_only_the_saved_browser_and_profile() {
    let root = tempdir().unwrap();
    let create_profile = |browser_root: &str, profile_id: &str, cookie_value: &str| {
        let user_data = root.path().join(browser_root);
        let profile = user_data.join(profile_id).join("Network");
        fs::create_dir_all(&profile).unwrap();
        let local_state = user_data.join("Local State");
        fs::write(local_state, "{}").unwrap();
        let database = profile.join("Cookies");
        let connection = Connection::open(database).unwrap();
        connection
                .execute_batch(
                    "CREATE TABLE cookies (host_key TEXT, name TEXT, value TEXT, encrypted_value BLOB, path TEXT, expires_utc INTEGER, last_access_utc INTEGER);",
                )
                .unwrap();
        connection
            .execute(
                "INSERT INTO cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    ".claude.ai",
                    "sessionKey",
                    cookie_value,
                    Vec::<u8>::new(),
                    "/",
                    0,
                    1
                ],
            )
            .unwrap();
    };
    create_profile("Google/Chrome/User Data", "Default", "chrome-default");
    create_profile("Google/Chrome/User Data", "Profile 2", "chrome-profile-two");
    create_profile("Microsoft/Edge/User Data", "Default", "edge-default");

    let importer = WindowsBrowserCookieImporter::with_local_app_data(root.path());
    let mut account =
        AccountRecord::create("Claude", "user@example.com", None, CLAUDE, None).unwrap();
    account.browser_kind = Some("chrome".to_owned());
    account.browser_profile_id = Some("Profile 2".to_owned());
    let material = importer.reimport(&account).await.unwrap().unwrap();
    assert_eq!(
        material.cookie_header().as_deref(),
        Some("sessionKey=chrome-profile-two")
    );

    account.browser_kind = None;
    assert!(importer.reimport(&account).await.unwrap().is_none());
}
