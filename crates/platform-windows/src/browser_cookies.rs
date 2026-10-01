//! One-shot Windows Chromium cookie import.
//!
//! The importer reads a browser's existing profile without starting a browser
//! process, copies the cookie database to a private temporary directory, and
//! returns only the selected session cookies. It never writes to the browser
//! profile and never keeps a browser/WebView alive. The caller should persist
//! the returned header through the account-scoped secure auth store.

use crate::{WindowsBrowserLauncher, WindowsDefaultBrowserLauncher};
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD as BASE64_STANDARD};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    ptr, slice,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tempfile::{TempDir, tempdir};
use thiserror::Error;
use url::Url;
use usage_monitor_core::{
    accounts::{AccountRecord, CLAUDE, OPENAI, OPENCODE_GO, OPENROUTER},
    auth::{
        AccountAuthMaterial, AccountAuthMaterialStore, AccountBrowserSessionRefresher, AuthError,
        CookieValue, OAuthBrowserLauncher,
    },
};
use windows_sys::Win32::{
    Foundation::{GetLastError, HLOCAL, LocalFree},
    Security::Cryptography::{CRYPT_INTEGER_BLOB, CryptUnprotectData},
};

const CHROME_COOKIE_EPOCH_OFFSET_MICROS: i64 = 11_644_473_600_000_000;
const MAX_COOKIE_HEADER_BYTES: usize = 256 * 1024;
const MAX_COOKIE_VALUE_BYTES: usize = 128 * 1024;

/// Chromium-family browsers whose profile layout and cookie encryption are
/// compatible with this importer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BrowserKind {
    Chrome,
    Edge,
    Brave,
    Chromium,
}

impl BrowserKind {
    pub const ALL: [Self; 4] = [Self::Chrome, Self::Edge, Self::Brave, Self::Chromium];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chrome => "chrome",
            Self::Edge => "edge",
            Self::Brave => "brave",
            Self::Chromium => "chromium",
        }
    }

    fn from_account_id(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|browser| browser.as_str().eq_ignore_ascii_case(value.trim()))
    }

    fn user_data_root(self, local_app_data: &Path) -> PathBuf {
        match self {
            Self::Chrome => local_app_data
                .join("Google")
                .join("Chrome")
                .join("User Data"),
            Self::Edge => local_app_data
                .join("Microsoft")
                .join("Edge")
                .join("User Data"),
            Self::Brave => local_app_data
                .join("BraveSoftware")
                .join("Brave-Browser")
                .join("User Data"),
            Self::Chromium => local_app_data.join("Chromium").join("User Data"),
        }
    }
}

impl std::fmt::Display for BrowserKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct BrowserProfile {
    pub browser: BrowserKind,
    pub profile_id: String,
    pub display_name: Option<String>,
    pub path: PathBuf,
}

impl BrowserProfile {
    fn cookies_path(&self) -> Option<PathBuf> {
        let network_path = self.path.join("Network").join("Cookies");
        if network_path.is_file() {
            return Some(network_path);
        }
        let legacy_path = self.path.join("Cookies");
        legacy_path.is_file().then_some(legacy_path)
    }

    fn local_state_path(&self) -> PathBuf {
        self.path.parent().map_or_else(
            || PathBuf::from("Local State"),
            |root| root.join("Local State"),
        )
    }
}

/// A single read-only browser import request. `required_cookie_names` uses
/// OR semantics: at least one named cookie must be present. With an empty list,
/// all non-expired cookies from the requested domains are returned.
#[derive(Debug, Clone)]
pub struct BrowserCookieImportRequest {
    pub browser: Option<BrowserKind>,
    pub profile_id: Option<String>,
    pub domains: Vec<String>,
    pub required_cookie_names: Vec<String>,
}

impl BrowserCookieImportRequest {
    pub fn new(domains: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            browser: None,
            profile_id: None,
            domains: domains.into_iter().map(Into::into).collect(),
            required_cookie_names: Vec::new(),
        }
    }

    /// Create the browser-cookie contract used by the built-in provider
    /// adapters. API-key providers are intentionally not mapped to browser
    /// cookies here; they must use their provider-owned key flow.
    pub fn for_provider(provider_id: &str) -> Result<Self, BrowserCookieError> {
        let normalized = provider_id.trim().to_ascii_lowercase();
        let (domains, required_cookie_names) = match normalized.as_str() {
            CLAUDE => (vec!["claude.ai".to_owned()], vec!["sessionKey".to_owned()]),
            OPENCODE_GO => (
                vec!["opencode.ai".to_owned(), "app.opencode.ai".to_owned()],
                vec![
                    "auth".to_owned(),
                    "__Host-auth".to_owned(),
                    "__Host-console_session".to_owned(),
                ],
            ),
            OPENAI => (
                vec!["chatgpt.com".to_owned(), "openai.com".to_owned()],
                vec![],
            ),
            OPENROUTER => (vec!["openrouter.ai".to_owned()], vec![]),
            _ => {
                return Err(BrowserCookieError::UnsupportedProvider(normalized));
            }
        };
        Ok(Self {
            browser: None,
            profile_id: None,
            domains,
            required_cookie_names,
        })
    }

    fn validate(&self) -> Result<(), BrowserCookieError> {
        if self.domains.is_empty()
            || self
                .domains
                .iter()
                .all(|domain| normalize_domain(domain).is_none())
        {
            return Err(BrowserCookieError::InvalidRequest(
                "at least one valid cookie domain is required".to_owned(),
            ));
        }
        if let Some(profile_id) = self.profile_id.as_deref()
            && !is_safe_profile_id(profile_id)
        {
            return Err(BrowserCookieError::InvalidRequest(
                "browser profile id contains invalid path characters".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct BrowserCookieImportResult {
    pub browser: BrowserKind,
    pub profile_id: String,
    pub display_name: Option<String>,
    pub cookie_header: String,
    pub cookies: Vec<CookieValue>,
    /// Chromium does not persist the browser's User-Agent in the cookie DB.
    /// The host may fill this from its own request layer when needed.
    pub user_agent: Option<String>,
}

/// Parameters for the user-driven browser login bridge.
///
/// The bridge deliberately uses the user's normal browser and does not create
/// a WebView or keep a browser process alive. It opens the provider's login
/// page once, then observes the existing Chromium profile until the provider's
/// session cookie becomes available.
#[derive(Debug, Clone)]
pub struct BrowserLoginOptions {
    pub browser: Option<BrowserKind>,
    pub profile_id: Option<String>,
    pub timeout: Duration,
    pub poll_interval: Duration,
}

impl Default for BrowserLoginOptions {
    fn default() -> Self {
        Self {
            browser: None,
            profile_id: None,
            timeout: Duration::from_secs(300),
            poll_interval: Duration::from_secs(1),
        }
    }
}

#[derive(Debug, Clone)]
pub struct BrowserLoginResult {
    pub provider_id: String,
    pub imported: BrowserCookieImportResult,
    pub elapsed: Duration,
    pub opened_browser: bool,
}

#[derive(Debug, Error)]
pub enum BrowserCookieError {
    #[error("LOCALAPPDATA is not available")]
    MissingLocalAppData,
    #[error("invalid browser cookie import request: {0}")]
    InvalidRequest(String),
    #[error("provider {0} has no browser-cookie contract")]
    UnsupportedProvider(String),
    #[error("browser profile was not found")]
    ProfileNotFound,
    #[error("no browser profile contains a matching session")]
    NoMatchingSession,
    #[error("browser session is ambiguous across {0} profiles; select a profile explicitly")]
    AmbiguousProfile(usize),
    #[error("browser cookie database was not found for profile")]
    DatabaseMissing,
    #[error(
        "browser profile is busy and its Cookies database cannot be read; close the browser and retry: {0}"
    )]
    BrowserProfileBusy(String),
    #[error("could not open the provider login page: {0}")]
    BrowserOpenFailed(String),
    #[error("timed out waiting for the {provider_id} browser session after {timeout_seconds}s")]
    LoginTimedOut {
        provider_id: String,
        timeout_seconds: u64,
    },
    #[error("could not copy browser cookie database: {0}")]
    DatabaseCopyFailed(String),
    #[error("could not query browser cookie database: {0}")]
    DatabaseQueryFailed(String),
    #[error("browser Local State is invalid: {0}")]
    LocalStateInvalid(String),
    #[error("browser cookie uses unsupported encryption {0}")]
    UnsupportedEncryption(String),
    #[error("browser cookie decryption failed: {0}")]
    DecryptionFailed(String),
    #[error("browser credential store failed: {0}")]
    CredentialStore(String),
}

/// Reads Chromium profiles from `%LOCALAPPDATA%` and imports a session once.
/// No browser executable is launched and no source profile is modified.
#[derive(Debug, Clone)]
pub struct WindowsBrowserCookieImporter {
    local_app_data: PathBuf,
}

impl WindowsBrowserCookieImporter {
    pub fn from_process() -> Result<Self, BrowserCookieError> {
        let local_app_data = env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or(BrowserCookieError::MissingLocalAppData)?;
        Ok(Self { local_app_data })
    }

    pub fn with_local_app_data(path: impl Into<PathBuf>) -> Self {
        Self {
            local_app_data: path.into(),
        }
    }

    pub fn local_app_data(&self) -> &Path {
        &self.local_app_data
    }

    pub fn discover_profiles(
        &self,
        browser: Option<BrowserKind>,
    ) -> Result<Vec<BrowserProfile>, BrowserCookieError> {
        let browsers = browser.map_or_else(|| BrowserKind::ALL.to_vec(), |kind| vec![kind]);
        let mut profiles = Vec::new();
        for browser in browsers {
            let root = browser.user_data_root(&self.local_app_data);
            if !root.is_dir() {
                continue;
            }
            let display_names = read_profile_display_names(&root);
            let entries = fs::read_dir(&root).map_err(|error| {
                BrowserCookieError::DatabaseCopyFailed(format!(
                    "could not enumerate profiles: {error}"
                ))
            })?;
            for entry in entries {
                let entry = entry.map_err(|error| {
                    BrowserCookieError::DatabaseCopyFailed(format!(
                        "could not read profile entry: {error}"
                    ))
                })?;
                let path = entry.path();
                if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                    continue;
                }
                let Some(profile_id) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !is_safe_profile_id(&profile_id) || profile_id == "System Profile" {
                    continue;
                }
                if !path.join("Network").join("Cookies").is_file()
                    && !path.join("Cookies").is_file()
                {
                    continue;
                }
                profiles.push(BrowserProfile {
                    browser,
                    display_name: display_names.get(&profile_id).cloned(),
                    profile_id,
                    path,
                });
            }
        }
        profiles.sort_by(|left, right| {
            left.browser
                .cmp(&right.browser)
                .then_with(|| left.profile_id.cmp(&right.profile_id))
        });
        Ok(profiles)
    }

    pub fn import(
        &self,
        request: &BrowserCookieImportRequest,
    ) -> Result<BrowserCookieImportResult, BrowserCookieError> {
        request.validate()?;
        let profiles = self.discover_profiles(request.browser)?;
        let candidates = profiles
            .into_iter()
            .filter(|profile| {
                request
                    .profile_id
                    .as_deref()
                    .is_none_or(|profile_id| profile.profile_id == profile_id)
            })
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return Err(BrowserCookieError::ProfileNotFound);
        }

        let mut successes = Vec::new();
        let mut first_error = None;
        for profile in candidates {
            match self.import_profile(&profile, request) {
                Ok(result) => successes.push(result),
                Err(error) => {
                    if !matches!(
                        error,
                        BrowserCookieError::NoMatchingSession | BrowserCookieError::DatabaseMissing
                    ) && first_error.is_none()
                    {
                        first_error = Some(error);
                    }
                }
            }
        }

        match successes.len() {
            1 => Ok(successes.remove(0)),
            count if count > 1 => Err(BrowserCookieError::AmbiguousProfile(count)),
            _ => Err(first_error.unwrap_or(BrowserCookieError::NoMatchingSession)),
        }
    }

    fn import_profile(
        &self,
        profile: &BrowserProfile,
        request: &BrowserCookieImportRequest,
    ) -> Result<BrowserCookieImportResult, BrowserCookieError> {
        let database = profile
            .cookies_path()
            .ok_or(BrowserCookieError::DatabaseMissing)?;
        let master_key = load_master_key(&profile.local_state_path())?;
        // Chromium normally permits a private copy, but some Windows builds
        // open the live cookie DB without FILE_SHARE_READ. Fall back to a
        // read-only SQLite snapshot of the live file so an already-running
        // browser can still be reused; neither path writes to the profile.
        let raw_cookies = match copy_cookie_database(&database) {
            Ok(copied) => read_cookie_rows(&copied.path)?,
            Err(copy_error) => match read_cookie_rows(&database) {
                Ok(rows) => rows,
                Err(read_error) => {
                    return Err(BrowserCookieError::DatabaseCopyFailed(format!(
                        "{copy_error}; direct read failed: {read_error}"
                    )));
                }
            },
        };
        let selected = select_cookies(raw_cookies, request, master_key.as_deref())?;
        if selected.is_empty() {
            return Err(BrowserCookieError::NoMatchingSession);
        }
        let cookies = selected
            .into_iter()
            .map(|(_, value)| CookieValue {
                name: value.0,
                value: value.1,
            })
            .collect::<Vec<_>>();
        let cookie_header = cookies
            .iter()
            .map(CookieValue::to_header_pair)
            .collect::<Vec<_>>()
            .join("; ");
        if cookie_header.len() > MAX_COOKIE_HEADER_BYTES {
            return Err(BrowserCookieError::DecryptionFailed(
                "selected cookie header is too large".to_owned(),
            ));
        }
        Ok(BrowserCookieImportResult {
            browser: profile.browser,
            profile_id: profile.profile_id.clone(),
            display_name: profile.display_name.clone(),
            cookie_header,
            cookies,
            user_agent: None,
        })
    }

    /// Open a provider login page in the user's default browser and wait for
    /// the browser session to appear in a supported Chromium profile.
    ///
    /// This is the account-add flow: the application never receives a
    /// password, never embeds a WebView, and never keeps the browser open for
    /// polling. Each poll reads a private copy of the cookie database and
    /// returns only the provider cookies required by the adapter.
    pub async fn open_and_wait_for_provider(
        &self,
        provider_id: &str,
        login_url: &Url,
        options: BrowserLoginOptions,
    ) -> Result<BrowserLoginResult, BrowserCookieError> {
        if !matches!(login_url.scheme(), "http" | "https") {
            return Err(BrowserCookieError::BrowserOpenFailed(
                "login URL must use HTTP(S)".to_owned(),
            ));
        }
        let mut request = BrowserCookieImportRequest::for_provider(provider_id)?;
        request.browser = options.browser;
        request.profile_id = options.profile_id.clone();

        let timeout = if options.timeout.is_zero() {
            BrowserLoginOptions::default().timeout
        } else {
            options.timeout
        };
        let poll_interval = if options.poll_interval.is_zero() {
            BrowserLoginOptions::default().poll_interval
        } else {
            options.poll_interval
        };

        let provider_id = provider_id.trim().to_ascii_lowercase();
        let started = Instant::now();

        // Reuse an existing browser session before opening any login page.
        // This is the normal path for a user who is already signed in.
        match self.import(&request) {
            Ok(imported) => {
                return Ok(BrowserLoginResult {
                    provider_id,
                    imported,
                    elapsed: started.elapsed(),
                    opened_browser: false,
                });
            }
            Err(error) if is_retryable_login_error(&error) => {}
            Err(error) if is_browser_profile_busy(&error) => {
                return Err(BrowserCookieError::BrowserProfileBusy(error.to_string()));
            }
            Err(error) => return Err(error),
        }

        let open_result = match options.browser {
            Some(browser) => WindowsBrowserLauncher::open(browser, login_url).await,
            None => WindowsDefaultBrowserLauncher.open(login_url).await,
        };
        open_result.map_err(|error| BrowserCookieError::BrowserOpenFailed(error.to_string()))?;

        loop {
            match self.import(&request) {
                Ok(imported) => {
                    return Ok(BrowserLoginResult {
                        provider_id,
                        imported,
                        elapsed: started.elapsed(),
                        opened_browser: true,
                    });
                }
                Err(error) if is_retryable_login_error(&error) => {}
                Err(error) if is_browser_profile_busy(&error) => {
                    return Err(BrowserCookieError::BrowserProfileBusy(error.to_string()));
                }
                Err(error) => return Err(error),
            }

            if started.elapsed() >= timeout {
                return Err(BrowserCookieError::LoginTimedOut {
                    provider_id,
                    timeout_seconds: timeout.as_secs().max(1),
                });
            }
            tokio::time::sleep(poll_interval).await;
        }
    }
}

#[async_trait]
impl AccountBrowserSessionRefresher for WindowsBrowserCookieImporter {
    async fn reimport(
        &self,
        account: &AccountRecord,
    ) -> Result<Option<AccountAuthMaterial>, AuthError> {
        let (Some(browser_id), Some(profile_id)) = (
            account.browser_kind.as_deref(),
            account.browser_profile_id.as_deref(),
        ) else {
            return Ok(None);
        };
        let browser = BrowserKind::from_account_id(browser_id).ok_or_else(|| {
            AuthError::CredentialStore("the saved browser type is not supported".to_owned())
        })?;
        let mut request = BrowserCookieImportRequest::for_provider(&account.provider_id)
            .map_err(|error| AuthError::CredentialStore(error.to_string()))?;
        request.browser = Some(browser);
        request.profile_id = Some(profile_id.to_owned());
        let imported = self
            .import(&request)
            .map_err(|error| AuthError::CredentialStore(error.to_string()))?;
        Ok(Some(AccountAuthMaterial {
            cookies: imported.cookies,
            user_agent: imported.user_agent,
            ..AccountAuthMaterial::default()
        }))
    }
}

fn is_retryable_login_error(error: &BrowserCookieError) -> bool {
    // Chromium can hold an exclusive handle on its Cookies database while a
    // browser window is open.  During an interactive add flow this is
    // recoverable: the user can finish sign-in and close that browser, after
    // which the next poll can read the same profile.  Non-interactive imports
    // still surface BrowserProfileBusy to their caller.
    matches!(
        error,
        BrowserCookieError::NoMatchingSession
            | BrowserCookieError::DatabaseCopyFailed(_)
            | BrowserCookieError::DatabaseQueryFailed(_)
    )
}

fn is_browser_profile_busy(error: &BrowserCookieError) -> bool {
    matches!(
        error,
        BrowserCookieError::DatabaseCopyFailed(_) | BrowserCookieError::DatabaseQueryFailed(_)
    )
}

/// Import and immediately persist a provider's browser session in the
/// account-scoped secure store. The browser database is closed before the
/// store is updated, and the returned cookie header is not logged.
pub async fn import_and_store_for_account<S>(
    importer: &WindowsBrowserCookieImporter,
    store: &S,
    account: &AccountRecord,
    browser: Option<BrowserKind>,
) -> Result<BrowserCookieImportResult, BrowserCookieError>
where
    S: AccountAuthMaterialStore + ?Sized,
{
    let mut request = BrowserCookieImportRequest::for_provider(&account.provider_id)?;
    request.browser = browser;
    request.profile_id = account.browser_profile_id.clone();
    let imported = importer.import(&request)?;
    let material = AccountAuthMaterial {
        cookies: imported.cookies.clone(),
        user_agent: imported.user_agent.clone(),
        ..AccountAuthMaterial::default()
    };
    let mut material = material;
    if let Some(existing) = store.get(account.id).await.map_err(auth_store_error)? {
        material.fill_missing_from(&existing);
    }
    store
        .save(account.id, &material)
        .await
        .map_err(auth_store_error)?;
    Ok(imported)
}

fn auth_store_error(error: AuthError) -> BrowserCookieError {
    BrowserCookieError::CredentialStore(error.to_string())
}

#[derive(Debug)]
struct TempCookieDatabase {
    _directory: TempDir,
    path: PathBuf,
}

#[derive(Debug)]
struct RawCookieRow {
    host: String,
    name: String,
    value: Option<String>,
    encrypted_value: Vec<u8>,
    path: String,
    expires_utc: i64,
    last_access_utc: i64,
}

fn copy_cookie_database(source: &Path) -> Result<TempCookieDatabase, BrowserCookieError> {
    let directory = tempdir().map_err(|error| {
        BrowserCookieError::DatabaseCopyFailed(format!(
            "could not create temporary directory: {error}"
        ))
    })?;
    let destination = directory.path().join("Cookies");
    fs::copy(source, &destination).map_err(|error| {
        BrowserCookieError::DatabaseCopyFailed(format!("could not copy cookie database: {error}"))
    })?;
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{}", source.display(), suffix));
        if sidecar.is_file() {
            fs::copy(&sidecar, directory.path().join(format!("Cookies{suffix}"))).map_err(
                |error| {
                    BrowserCookieError::DatabaseCopyFailed(format!(
                        "could not copy cookie database sidecar: {error}"
                    ))
                },
            )?;
        }
    }
    Ok(TempCookieDatabase {
        _directory: directory,
        path: destination,
    })
}

fn read_cookie_rows(path: &Path) -> Result<Vec<RawCookieRow>, BrowserCookieError> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|error| BrowserCookieError::DatabaseQueryFailed(error.to_string()))?;
    let mut statement = connection
        .prepare(
            "SELECT host_key, name, value, encrypted_value, path, expires_utc, last_access_utc FROM cookies",
        )
        .map_err(|error| BrowserCookieError::DatabaseQueryFailed(error.to_string()))?;
    let rows = statement
        .query_map([], |row| {
            Ok(RawCookieRow {
                host: row.get(0)?,
                name: row.get(1)?,
                value: row.get(2)?,
                encrypted_value: row.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
                path: row.get(4)?,
                expires_utc: row.get(5).unwrap_or_default(),
                last_access_utc: row.get(6).unwrap_or_default(),
            })
        })
        .map_err(|error| BrowserCookieError::DatabaseQueryFailed(error.to_string()))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| BrowserCookieError::DatabaseQueryFailed(error.to_string()))
}

fn select_cookies(
    rows: Vec<RawCookieRow>,
    request: &BrowserCookieImportRequest,
    master_key: Option<&[u8]>,
) -> Result<BTreeMap<String, (String, String)>, BrowserCookieError> {
    let domains = request
        .domains
        .iter()
        .filter_map(|domain| normalize_domain(domain))
        .collect::<Vec<_>>();
    let required = request
        .required_cookie_names
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    let now = chrome_now_micros();
    let mut selected = BTreeMap::<String, SelectedCookie>::new();
    let mut first_crypto_error = None;
    for row in rows {
        if !domains.iter().any(|domain| host_matches(&row.host, domain))
            || (!required.is_empty() && !required.contains(&row.name.to_ascii_lowercase()))
            || (row.expires_utc > 0 && row.expires_utc <= now)
        {
            continue;
        }
        let value =
            match decode_cookie_value(row.value.as_deref(), &row.encrypted_value, master_key) {
                Ok(value) => value,
                Err(error @ BrowserCookieError::UnsupportedEncryption(_))
                | Err(error @ BrowserCookieError::DecryptionFailed(_)) => {
                    if first_crypto_error.is_none() {
                        first_crypto_error = Some(error);
                    }
                    continue;
                }
                Err(error) => return Err(error),
            };
        if value.is_empty() || value.len() > MAX_COOKIE_VALUE_BYTES {
            continue;
        }
        let candidate = SelectedCookie {
            name: row.name.clone(),
            value,
            host_exactness: host_exactness(&row.host, &domains),
            path_len: row.path.len(),
            last_access_utc: row.last_access_utc,
        };
        let key = row.name.to_ascii_lowercase();
        if selected
            .get(&key)
            .is_none_or(|existing| candidate.is_better_than(existing))
        {
            selected.insert(key, candidate);
        }
    }
    if selected.is_empty()
        && let Some(error) = first_crypto_error
    {
        return Err(error);
    }
    Ok(selected
        .into_iter()
        .map(|(key, value)| (key, (value.name, value.value)))
        .collect())
}

#[derive(Debug)]
struct SelectedCookie {
    name: String,
    value: String,
    host_exactness: (bool, usize),
    path_len: usize,
    last_access_utc: i64,
}

impl SelectedCookie {
    fn is_better_than(&self, other: &Self) -> bool {
        (
            self.host_exactness.0,
            self.host_exactness.1,
            self.path_len,
            self.last_access_utc,
        ) > (
            other.host_exactness.0,
            other.host_exactness.1,
            other.path_len,
            other.last_access_utc,
        )
    }
}

fn decode_cookie_value(
    plaintext: Option<&str>,
    encrypted: &[u8],
    master_key: Option<&[u8]>,
) -> Result<String, BrowserCookieError> {
    if let Some(value) = plaintext.filter(|value| !value.is_empty()) {
        return Ok(value.to_owned());
    }
    if encrypted.is_empty() {
        return Ok(String::new());
    }
    if encrypted.starts_with(b"v20") {
        return Err(BrowserCookieError::UnsupportedEncryption(
            "v20 (app-bound)".to_owned(),
        ));
    }
    let bytes = if encrypted.starts_with(b"v10") || encrypted.starts_with(b"v11") {
        let key = master_key.ok_or_else(|| {
            BrowserCookieError::DecryptionFailed("Chromium master key is unavailable".to_owned())
        })?;
        if key.len() != 32 || encrypted.len() < 3 + 12 + 16 {
            return Err(BrowserCookieError::DecryptionFailed(
                "invalid Chromium AES-GCM payload".to_owned(),
            ));
        }
        let unbound = UnboundKey::new(&AES_256_GCM, key).map_err(|_| {
            BrowserCookieError::DecryptionFailed("invalid Chromium AES-GCM key".to_owned())
        })?;
        let key = LessSafeKey::new(unbound);
        let nonce = Nonce::try_assume_unique_for_key(&encrypted[3..15]).map_err(|_| {
            BrowserCookieError::DecryptionFailed("invalid Chromium AES-GCM nonce".to_owned())
        })?;
        let mut payload = encrypted[15..].to_vec();
        key.open_in_place(nonce, Aad::empty(), &mut payload)
            .map_err(|_| {
                BrowserCookieError::DecryptionFailed(
                    "Chromium AES-GCM authentication failed".to_owned(),
                )
            })?
            .to_vec()
    } else {
        dpapi_unprotect(encrypted)?
    };
    if bytes.len() > MAX_COOKIE_VALUE_BYTES {
        return Err(BrowserCookieError::DecryptionFailed(
            "decrypted cookie value is too large".to_owned(),
        ));
    }
    String::from_utf8(bytes)
        .map_err(|_| BrowserCookieError::DecryptionFailed("cookie value is not UTF-8".to_owned()))
}

fn load_master_key(local_state_path: &Path) -> Result<Option<Vec<u8>>, BrowserCookieError> {
    let bytes = match fs::read(local_state_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BrowserCookieError::LocalStateInvalid(error.to_string()));
        }
    };
    let root: Value = serde_json::from_slice(&bytes)
        .map_err(|error| BrowserCookieError::LocalStateInvalid(error.to_string()))?;
    let Some(encoded) = root
        .pointer("/os_crypt/encrypted_key")
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let mut encrypted = BASE64_STANDARD
        .decode(encoded)
        .map_err(|error| BrowserCookieError::LocalStateInvalid(error.to_string()))?;
    if encrypted.starts_with(b"DPAPI") {
        encrypted.drain(..5);
    }
    let key = dpapi_unprotect(&encrypted)?;
    if key.len() != 32 {
        return Err(BrowserCookieError::LocalStateInvalid(
            "Chromium master key is not 256-bit".to_owned(),
        ));
    }
    Ok(Some(key))
}

fn dpapi_unprotect(input: &[u8]) -> Result<Vec<u8>, BrowserCookieError> {
    if input.is_empty() {
        return Err(BrowserCookieError::DecryptionFailed(
            "DPAPI input is empty".to_owned(),
        ));
    }
    let input_blob = CRYPT_INTEGER_BLOB {
        cbData: input.len() as u32,
        pbData: input.as_ptr() as *mut u8,
    };
    let mut output_blob = CRYPT_INTEGER_BLOB::default();
    let success = unsafe {
        CryptUnprotectData(
            &input_blob,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            0,
            &mut output_blob,
        )
    };
    if success == 0 {
        return Err(BrowserCookieError::DecryptionFailed(format!(
            "DPAPI failed with Win32 error {}",
            unsafe { GetLastError() }
        )));
    }
    if output_blob.cbData == 0 || output_blob.pbData.is_null() {
        if !output_blob.pbData.is_null() {
            unsafe {
                let _ = LocalFree(output_blob.pbData as HLOCAL);
            }
        }
        return Err(BrowserCookieError::DecryptionFailed(
            "DPAPI returned an empty value".to_owned(),
        ));
    }
    let bytes =
        unsafe { slice::from_raw_parts(output_blob.pbData, output_blob.cbData as usize).to_vec() };
    unsafe {
        let _ = LocalFree(output_blob.pbData as HLOCAL);
    }
    Ok(bytes)
}

fn read_profile_display_names(root: &Path) -> BTreeMap<String, String> {
    let path = root.join("Local State");
    let Ok(bytes) = fs::read(path) else {
        return BTreeMap::new();
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return BTreeMap::new();
    };
    value
        .pointer("/profile/info_cache")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|entries| entries.iter())
        .filter_map(|(profile_id, entry)| {
            let name = entry.get("name").and_then(Value::as_str)?.trim();
            (!name.is_empty()).then_some((profile_id.clone(), name.to_owned()))
        })
        .collect()
}

fn is_safe_profile_id(value: &str) -> bool {
    !value.trim().is_empty() && value != "." && value != ".." && !value.contains(['\\', '/', ':'])
}

fn normalize_domain(value: &str) -> Option<String> {
    let value = value.trim().trim_start_matches('.').to_ascii_lowercase();
    (!value.is_empty() && !value.contains(['/', '\\', ':'])).then_some(value)
}

fn host_matches(host: &str, domain: &str) -> bool {
    let host = host.trim().trim_start_matches('.').to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

fn host_exactness(host: &str, domains: &[String]) -> (bool, usize) {
    let host = host.trim().trim_start_matches('.').to_ascii_lowercase();
    domains
        .iter()
        .filter(|domain| host_matches(&host, domain))
        .map(|domain| (host == *domain, domain.len()))
        .max()
        .unwrap_or((false, 0))
}

fn chrome_now_micros() -> i64 {
    let unix_micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros().min(i64::MAX as u128) as i64)
        .unwrap_or_default();
    unix_micros.saturating_add(CHROME_COOKIE_EPOCH_OFFSET_MICROS)
}

#[cfg(test)]
mod tests {
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
        let selected =
            select_cookies(rows, &request(&["claude.ai"], &["sessionKey"]), None).unwrap();
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
        let selected =
            select_cookies(rows, &request(&["claude.ai"], &["sessionKey"]), None).unwrap();
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
}
