use async_trait::async_trait;
use codex_usage_core::{
    accounts::AccountId,
    auth::{
        AccountAuthMaterial, AccountAuthMaterialStore, AuthError, OAuthBrowserLauncher,
        OAuthCredentialStore, StoredOAuthCredential,
    },
};
use std::{env, path::PathBuf, process::Command};
use std::{ffi::c_void, ptr, slice};
use url::Url;
use windows_sys::Win32::{
    Foundation::{ERROR_NOT_FOUND, FILETIME, GetLastError},
    Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
        CredReadW, CredWriteW,
    },
    UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
};

pub mod browser_bridge;
pub mod browser_cookies;

const TARGET_PREFIX: &str = "CodexUsageMonitor-Rust/OAuth/";
const AUTH_MATERIAL_TARGET_PREFIX: &str = "CodexUsageMonitor-Rust/Auth/";
const MAX_BLOB_BYTES: usize = 5 * 1024;
const MAX_AUTH_MATERIAL_BLOB_BYTES: usize = 64 * 1024;

pub struct WindowsCredentialManagerStore;

impl WindowsCredentialManagerStore {
    fn load(account_id: AccountId) -> Result<Option<StoredOAuthCredential>, AuthError> {
        load_blob(TARGET_PREFIX, account_id, MAX_BLOB_BYTES)?
            .map(|bytes| {
                serde_json::from_slice(&bytes).map_err(|error| {
                    AuthError::CredentialStore(format!("stored credential is invalid: {error}"))
                })
            })
            .transpose()
    }

    fn save(account_id: AccountId, credential: &StoredOAuthCredential) -> Result<(), AuthError> {
        let blob = serde_json::to_vec(credential).map_err(|error| {
            AuthError::CredentialStore(format!("could not serialize credential: {error}"))
        })?;
        save_blob(TARGET_PREFIX, account_id, &blob, MAX_BLOB_BYTES)
    }

    fn remove(account_id: AccountId) -> Result<(), AuthError> {
        remove_blob(TARGET_PREFIX, account_id)
    }
}

#[async_trait]
impl OAuthCredentialStore for WindowsCredentialManagerStore {
    async fn get(&self, account_id: AccountId) -> Result<Option<StoredOAuthCredential>, AuthError> {
        Self::load(account_id)
    }

    async fn save(
        &self,
        account_id: AccountId,
        credential: &StoredOAuthCredential,
    ) -> Result<(), AuthError> {
        Self::save(account_id, credential)
    }

    async fn remove(&self, account_id: AccountId) -> Result<(), AuthError> {
        Self::remove(account_id)
    }
}

/// Windows Credential Manager store for imported browser cookies and provider
/// API keys. It is intentionally a different target namespace from OAuth
/// refresh credentials, so removing one kind cannot delete the other.
pub struct WindowsCredentialManagerAuthMaterialStore;

#[async_trait]
impl AccountAuthMaterialStore for WindowsCredentialManagerAuthMaterialStore {
    async fn get(&self, account_id: AccountId) -> Result<Option<AccountAuthMaterial>, AuthError> {
        load_blob(
            AUTH_MATERIAL_TARGET_PREFIX,
            account_id,
            MAX_AUTH_MATERIAL_BLOB_BYTES,
        )?
        .map(|bytes| {
            serde_json::from_slice(&bytes).map_err(|error| {
                AuthError::CredentialStore(format!(
                    "stored authentication material is invalid: {error}"
                ))
            })
        })
        .transpose()
    }

    async fn save(
        &self,
        account_id: AccountId,
        material: &AccountAuthMaterial,
    ) -> Result<(), AuthError> {
        if material.is_empty() {
            return Err(AuthError::CredentialStore(
                "refusing to persist empty authentication material".to_owned(),
            ));
        }
        let blob = serde_json::to_vec(material).map_err(|error| {
            AuthError::CredentialStore(format!(
                "could not serialize authentication material: {error}"
            ))
        })?;
        save_blob(
            AUTH_MATERIAL_TARGET_PREFIX,
            account_id,
            &blob,
            MAX_AUTH_MATERIAL_BLOB_BYTES,
        )
    }

    async fn remove(&self, account_id: AccountId) -> Result<(), AuthError> {
        remove_blob(AUTH_MATERIAL_TARGET_PREFIX, account_id)
    }
}

fn target(prefix: &str, account_id: AccountId) -> Vec<u16> {
    format!("{prefix}{account_id}\0").encode_utf16().collect()
}

fn load_blob(
    prefix: &str,
    account_id: AccountId,
    max_blob_bytes: usize,
) -> Result<Option<Vec<u8>>, AuthError> {
    let target = target(prefix, account_id);
    let mut raw: *mut CREDENTIALW = ptr::null_mut();
    let success = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut raw) };
    if success == 0 {
        let error = unsafe { GetLastError() };
        if error == ERROR_NOT_FOUND {
            return Ok(None);
        }
        return Err(AuthError::CredentialStore(format!(
            "CredReadW failed with Win32 error {error}"
        )));
    }
    if raw.is_null() {
        return Err(AuthError::CredentialStore(
            "CredReadW returned null".to_owned(),
        ));
    }
    let result = (|| {
        let credential = unsafe { &*raw };
        let size = credential.CredentialBlobSize as usize;
        if size > max_blob_bytes || (size > 0 && credential.CredentialBlob.is_null()) {
            return Err(AuthError::CredentialStore(
                "stored credential blob is invalid".to_owned(),
            ));
        }
        let bytes = if size == 0 {
            &[][..]
        } else {
            unsafe { slice::from_raw_parts(credential.CredentialBlob, size) }
        };
        Ok(Some(bytes.to_vec()))
    })();
    unsafe { CredFree(raw as *const c_void) };
    result
}

fn save_blob(
    prefix: &str,
    account_id: AccountId,
    blob: &[u8],
    max_blob_bytes: usize,
) -> Result<(), AuthError> {
    if blob.is_empty() || blob.len() > max_blob_bytes {
        return Err(AuthError::CredentialStore(
            "credential blob is empty or too large".to_owned(),
        ));
    }
    let mut blob = blob.to_vec();
    let mut target = target(prefix, account_id);
    let user_name: Vec<u16> = "CodexUsageMonitor\0".encode_utf16().collect();
    let credential = CREDENTIALW {
        Flags: 0,
        Type: CRED_TYPE_GENERIC,
        TargetName: target.as_mut_ptr(),
        Comment: ptr::null_mut(),
        LastWritten: FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        },
        CredentialBlobSize: blob.len() as u32,
        CredentialBlob: blob.as_mut_ptr(),
        Persist: CRED_PERSIST_LOCAL_MACHINE,
        AttributeCount: 0,
        Attributes: ptr::null_mut(),
        TargetAlias: ptr::null_mut(),
        UserName: user_name.as_ptr() as *mut u16,
    };
    let success = unsafe { CredWriteW(&credential, 0) };
    if success == 0 {
        return Err(AuthError::CredentialStore(format!(
            "CredWriteW failed with Win32 error {}",
            unsafe { GetLastError() }
        )));
    }
    Ok(())
}

fn remove_blob(prefix: &str, account_id: AccountId) -> Result<(), AuthError> {
    let target = target(prefix, account_id);
    let success = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
    if success == 0 {
        let error = unsafe { GetLastError() };
        if error == ERROR_NOT_FOUND {
            return Ok(());
        }
        return Err(AuthError::CredentialStore(format!(
            "CredDeleteW failed with Win32 error {error}"
        )));
    }
    Ok(())
}

pub struct WindowsDefaultBrowserLauncher;

#[async_trait]
impl OAuthBrowserLauncher for WindowsDefaultBrowserLauncher {
    async fn open(&self, authorization_uri: &Url) -> Result<(), AuthError> {
        if !matches!(authorization_uri.scheme(), "http" | "https") {
            return Err(AuthError::Config("browser URL must use HTTP(S)".to_owned()));
        }
        let operation: Vec<u16> = "open\0".encode_utf16().collect();
        let url: Vec<u16> = format!("{}\0", authorization_uri.as_str())
            .encode_utf16()
            .collect();
        let result = unsafe {
            ShellExecuteW(
                ptr::null_mut(),
                operation.as_ptr(),
                url.as_ptr(),
                ptr::null(),
                ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if (result as isize) <= 32 {
            return Err(AuthError::Callback(format!(
                "could not open default browser (ShellExecuteW={})",
                result as isize
            )));
        }
        Ok(())
    }
}

/// Opens a specific supported Chromium browser for an account-add flow.
///
/// The cookie importer can scan a selected browser/profile, so opening the
/// same browser is important when another browser is the Windows default.  The
/// spawned browser remains owned by the browser itself; this process does not
/// keep a child handle or a WebView alive.
pub struct WindowsBrowserLauncher;

impl WindowsBrowserLauncher {
    pub async fn open(browser: browser_cookies::BrowserKind, url: &Url) -> Result<(), AuthError> {
        let executable = browser_executable(browser).unwrap_or_else(|| {
            PathBuf::from(match browser {
                browser_cookies::BrowserKind::Chrome => "chrome.exe",
                browser_cookies::BrowserKind::Edge => "msedge.exe",
                browser_cookies::BrowserKind::Brave => "brave.exe",
                browser_cookies::BrowserKind::Chromium => "chromium.exe",
            })
        });
        Command::new(&executable)
            .arg(url.as_str())
            .spawn()
            .map(|_| ())
            .map_err(|error| {
                AuthError::Callback(format!(
                    "could not open {} ({}): {error}",
                    browser.as_str(),
                    executable.display()
                ))
            })
    }
}

fn browser_executable(browser: browser_cookies::BrowserKind) -> Option<PathBuf> {
    let local_app_data = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let program_files = env::var_os("PROGRAMFILES").map(PathBuf::from);
    let program_files_x86 = env::var_os("PROGRAMFILES(X86)").map(PathBuf::from);
    let mut candidates = Vec::new();
    match browser {
        browser_cookies::BrowserKind::Chrome => {
            if let Some(root) = local_app_data {
                candidates.push(root.join("Google/Chrome/Application/chrome.exe"));
            }
            for root in [program_files, program_files_x86].into_iter().flatten() {
                candidates.push(root.join("Google/Chrome/Application/chrome.exe"));
            }
        }
        browser_cookies::BrowserKind::Edge => {
            for root in [program_files_x86, program_files, local_app_data]
                .into_iter()
                .flatten()
            {
                candidates.push(root.join("Microsoft/Edge/Application/msedge.exe"));
            }
        }
        browser_cookies::BrowserKind::Brave => {
            for root in [program_files, program_files_x86, local_app_data]
                .into_iter()
                .flatten()
            {
                candidates.push(root.join("BraveSoftware/Brave-Browser/Application/brave.exe"));
            }
        }
        browser_cookies::BrowserKind::Chromium => {
            for root in [program_files, program_files_x86, local_app_data]
                .into_iter()
                .flatten()
            {
                candidates.push(root.join("Chromium/Application/chrome.exe"));
            }
        }
    }
    candidates.into_iter().find(|path| path.is_file())
}
