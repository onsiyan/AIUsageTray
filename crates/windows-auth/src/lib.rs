use async_trait::async_trait;
use codex_usage_core::{
    accounts::AccountId,
    auth::{
        AccountAuthMaterial, AccountAuthMaterialStore, AuthError, OAuthBrowserLauncher,
        OAuthCredentialStore, OAuthTokenSet, StoredOAuthCredential,
    },
    codex_desktop::{self, CodexDesktopPaths},
};
use std::{
    env,
    path::PathBuf,
    process::Command,
    sync::{Mutex, MutexGuard, OnceLock},
};
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
static AUTH_MATERIAL_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

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

/// Credentials of a Codex account linked to the Codex desktop app follow the
/// app's `auth.json` (see `codex_desktop`), so both sides always hold the one
/// valid single-use refresh token.
#[async_trait]
impl OAuthCredentialStore for WindowsCredentialManagerStore {
    async fn get(&self, account_id: AccountId) -> Result<Option<StoredOAuthCredential>, AuthError> {
        let Some(mut credential) = Self::load(account_id)? else {
            return Ok(None);
        };
        let paths = CodexDesktopPaths::from_environment();
        if codex_desktop::overlay_linked_credential(&paths, account_id, &mut credential) {
            Self::save(account_id, &credential)?;
        }
        Ok(Some(credential))
    }

    async fn save(
        &self,
        account_id: AccountId,
        credential: &StoredOAuthCredential,
    ) -> Result<(), AuthError> {
        Self::save(account_id, credential)?;
        codex_desktop::propagate_linked_credential(
            &CodexDesktopPaths::from_environment(),
            account_id,
            credential,
        )
    }

    async fn remove(&self, account_id: AccountId) -> Result<(), AuthError> {
        codex_desktop::forget_link_for(&CodexDesktopPaths::from_environment(), account_id);
        Self::remove(account_id)
    }

    async fn current_access_token(
        &self,
        account_id: AccountId,
    ) -> Result<Option<OAuthTokenSet>, AuthError> {
        Ok(codex_desktop::linked_access_token(
            &CodexDesktopPaths::from_environment(),
            account_id,
        ))
    }
}

/// Windows Credential Manager store for imported browser cookies and provider
/// API keys. It is intentionally a different target namespace from OAuth
/// refresh credentials, so removing one kind cannot delete the other.
pub struct WindowsCredentialManagerAuthMaterialStore;

#[async_trait]
impl AccountAuthMaterialStore for WindowsCredentialManagerAuthMaterialStore {
    async fn get(&self, account_id: AccountId) -> Result<Option<AccountAuthMaterial>, AuthError> {
        let _guard = auth_material_store_lock()?;
        load_auth_material(account_id)
    }

    async fn save(
        &self,
        account_id: AccountId,
        material: &AccountAuthMaterial,
    ) -> Result<(), AuthError> {
        let _guard = auth_material_store_lock()?;
        save_auth_material(account_id, material)
    }

    async fn remove(&self, account_id: AccountId) -> Result<(), AuthError> {
        let _guard = auth_material_store_lock()?;
        remove_blob(AUTH_MATERIAL_TARGET_PREFIX, account_id)
    }

    async fn replace_cookie_if_matches(
        &self,
        account_id: AccountId,
        cookie_name: &str,
        expected_value: &str,
        replacement_value: &str,
    ) -> Result<bool, AuthError> {
        if cookie_name.trim().is_empty()
            || expected_value.is_empty()
            || replacement_value.trim().is_empty()
        {
            return Ok(false);
        }

        let _guard = auth_material_store_lock()?;
        let Some(mut material) = load_auth_material(account_id)? else {
            return Ok(false);
        };
        let Some(cookie) = material
            .cookies
            .iter_mut()
            .find(|cookie| cookie.name.eq_ignore_ascii_case(cookie_name))
        else {
            return Ok(false);
        };
        if cookie.value != expected_value {
            return Ok(false);
        }
        cookie.value = replacement_value.to_owned();
        save_auth_material(account_id, &material)?;
        Ok(true)
    }
}

fn auth_material_store_lock() -> Result<MutexGuard<'static, ()>, AuthError> {
    AUTH_MATERIAL_STORE_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| {
            AuthError::CredentialStore("authentication material store is unavailable".to_owned())
        })
}

fn load_auth_material(account_id: AccountId) -> Result<Option<AccountAuthMaterial>, AuthError> {
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

fn save_auth_material(
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

/// Windows rejects generic credential blobs above `CRED_MAX_CREDENTIAL_BLOB_SIZE`
/// (5 * 512 bytes) with Win32 error 1783.
const CRED_MAX_BLOB_BYTES: usize = 5 * 512;
/// Marks a primary entry whose value is split across `#partN` entries.
const CHUNK_HEADER: &[u8] = b"CodexUsageMonitor-Chunks:";

fn target_name(prefix: &str, account_id: AccountId) -> String {
    format!("{prefix}{account_id}")
}

fn part_name(base: &str, index: usize) -> String {
    format!("{base}#part{index}")
}

/// Returns the number of parts when `primary` is a chunk header.
fn chunk_count(primary: &[u8]) -> Option<usize> {
    std::str::from_utf8(primary.strip_prefix(CHUNK_HEADER)?)
        .ok()?
        .parse::<usize>()
        .ok()
        .filter(|count| *count > 0)
}

fn load_blob(
    prefix: &str,
    account_id: AccountId,
    max_blob_bytes: usize,
) -> Result<Option<Vec<u8>>, AuthError> {
    let base = target_name(prefix, account_id);
    let Some(primary) = read_raw(&base)? else {
        return Ok(None);
    };
    let Some(count) = chunk_count(&primary) else {
        if primary.len() > max_blob_bytes {
            return Err(AuthError::CredentialStore(
                "stored credential blob is invalid".to_owned(),
            ));
        }
        return Ok(Some(primary));
    };
    if count > max_blob_bytes.div_ceil(CRED_MAX_BLOB_BYTES) {
        return Err(AuthError::CredentialStore(
            "stored credential blob is invalid".to_owned(),
        ));
    }
    let mut blob = Vec::new();
    for index in 0..count {
        let part = read_raw(&part_name(&base, index))?.ok_or_else(|| {
            AuthError::CredentialStore("stored credential blob is incomplete".to_owned())
        })?;
        blob.extend_from_slice(&part);
    }
    if blob.len() > max_blob_bytes {
        return Err(AuthError::CredentialStore(
            "stored credential blob is invalid".to_owned(),
        ));
    }
    Ok(Some(blob))
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
    let base = target_name(prefix, account_id);
    let previous_parts = read_raw(&base)?
        .as_deref()
        .and_then(chunk_count)
        .unwrap_or(0);
    let new_parts = if blob.len() <= CRED_MAX_BLOB_BYTES && !blob.starts_with(CHUNK_HEADER) {
        write_raw(&base, blob)?;
        0
    } else {
        // Write every part before the header so a reader never follows a
        // header to parts that do not exist yet.
        let chunks = blob.chunks(CRED_MAX_BLOB_BYTES).collect::<Vec<_>>();
        for (index, chunk) in chunks.iter().enumerate() {
            write_raw(&part_name(&base, index), chunk)?;
        }
        let mut header = CHUNK_HEADER.to_vec();
        header.extend_from_slice(chunks.len().to_string().as_bytes());
        write_raw(&base, &header)?;
        chunks.len()
    };
    for index in new_parts..previous_parts {
        delete_raw(&part_name(&base, index))?;
    }
    Ok(())
}

fn remove_blob(prefix: &str, account_id: AccountId) -> Result<(), AuthError> {
    let base = target_name(prefix, account_id);
    let parts = read_raw(&base)
        .ok()
        .flatten()
        .as_deref()
        .and_then(chunk_count)
        .unwrap_or(0);
    delete_raw(&base)?;
    for index in 0..parts {
        delete_raw(&part_name(&base, index))?;
    }
    Ok(())
}

fn wide(value: &str) -> Vec<u16> {
    format!("{value}\0").encode_utf16().collect()
}

fn read_raw(name: &str) -> Result<Option<Vec<u8>>, AuthError> {
    let target = wide(name);
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
        if size > MAX_AUTH_MATERIAL_BLOB_BYTES || (size > 0 && credential.CredentialBlob.is_null())
        {
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

fn write_raw(name: &str, blob: &[u8]) -> Result<(), AuthError> {
    let mut blob = blob.to_vec();
    let mut target = wide(name);
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

fn delete_raw(name: &str) -> Result<(), AuthError> {
    let target = wide(name);
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

#[cfg(test)]
mod blob_tests {
    use super::*;

    #[test]
    fn chunk_header_round_trips_and_rejects_plain_json() {
        let mut header = CHUNK_HEADER.to_vec();
        header.extend_from_slice(b"3");
        assert_eq!(chunk_count(&header), Some(3));
        assert_eq!(chunk_count(br#"{"refresh_token":"x"}"#), None);
        assert_eq!(chunk_count(CHUNK_HEADER), None);
    }

    /// Writes to the real Windows Credential Manager under a temporary
    /// target and removes it afterwards. Run explicitly with `--ignored`.
    #[test]
    #[ignore]
    fn blobs_larger_than_the_windows_limit_round_trip() {
        let account_id = AccountId::new();
        let prefix = "CodexUsageMonitor-Rust/Test/";
        let large = (0..(CRED_MAX_BLOB_BYTES * 2 + 17))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        save_blob(prefix, account_id, &large, MAX_AUTH_MATERIAL_BLOB_BYTES).unwrap();
        assert_eq!(
            load_blob(prefix, account_id, MAX_AUTH_MATERIAL_BLOB_BYTES).unwrap(),
            Some(large)
        );
        save_blob(prefix, account_id, b"small", MAX_AUTH_MATERIAL_BLOB_BYTES).unwrap();
        assert_eq!(
            load_blob(prefix, account_id, MAX_AUTH_MATERIAL_BLOB_BYTES).unwrap(),
            Some(b"small".to_vec())
        );
        let base = target_name(prefix, account_id);
        assert!(read_raw(&part_name(&base, 0)).unwrap().is_none());
        remove_blob(prefix, account_id).unwrap();
        assert!(read_raw(&base).unwrap().is_none());
    }
}
