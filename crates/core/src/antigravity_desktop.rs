//! Hands a saved Antigravity account to the Antigravity desktop app.
//!
//! The app (2.x) keeps its Google sign-in as JSON in the OS credential store
//! (`gemini:antigravity` on Windows). Saved Antigravity accounts use the same
//! Google OAuth client, and Google does not rotate refresh tokens, so the app
//! and this monitor can share one sign-in without any linking: switching only
//! writes the account's tokens there and restarts the app.

use crate::auth::{AuthError, OAuthTokenSet};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::SecondsFormat;
use serde_json::{Value, json};

/// Credential store entry the app reads its sign-in from.
pub const APP_CREDENTIAL_TARGET: &str = "gemini:antigravity";
pub const APP_CREDENTIAL_USER: &str = "antigravity";
/// Windows Credential Manager's limit for one entry's secret.
const MAX_CREDENTIAL_BYTES: usize = 2560;

/// The app's credential JSON for `tokens`. The optional `id_token` is left
/// out when it would push the entry past the credential store's size limit.
pub fn app_credential(tokens: &OAuthTokenSet) -> Result<Vec<u8>, AuthError> {
    let refresh_token = tokens.refresh_token.as_deref().ok_or_else(|| {
        AuthError::CredentialStore("the account has no refresh token to hand over".into())
    })?;
    let mut payload = json!({
        "token": {
            "access_token": tokens.access_token,
            "token_type": "Bearer",
            "refresh_token": refresh_token,
            "expiry": tokens.expires_at_utc.to_rfc3339_opts(SecondsFormat::Micros, true),
        },
        "auth_method": "consumer",
    });
    if let Some(id_token) = &tokens.id_token {
        payload["id_token"] = Value::String(id_token.clone());
        let bytes = serde_json::to_vec(&payload).map_err(serialize_error)?;
        if bytes.len() <= MAX_CREDENTIAL_BYTES {
            return Ok(bytes);
        }
        payload
            .as_object_mut()
            .expect("object payload")
            .remove("id_token");
    }
    let bytes = serde_json::to_vec(&payload).map_err(serialize_error)?;
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        return Err(AuthError::CredentialStore(
            "the Antigravity sign-in is too large for the credential store".into(),
        ));
    }
    Ok(bytes)
}

/// The email the app is signed in with, from its credential's `id_token`.
pub fn app_credential_email(credential: &[u8]) -> Option<String> {
    let value: Value = serde_json::from_slice(credential).ok()?;
    let payload = value.get("id_token")?.as_str()?.split('.').nth(1)?;
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?)
            .ok()?;
    claims
        .get("email")?
        .as_str()
        .map(|email| email.trim().to_ascii_lowercase())
}

fn serialize_error(error: serde_json::Error) -> AuthError {
    AuthError::CredentialStore(format!("could not build the Antigravity sign-in: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn tokens(id_token: Option<String>) -> OAuthTokenSet {
        OAuthTokenSet {
            access_token: "ya29.access".to_owned(),
            expires_at_utc: Utc.with_ymd_and_hms(2026, 10, 1, 7, 44, 19).unwrap(),
            refresh_token: Some("1//refresh".to_owned()),
            id_token,
            token_type: "Bearer".to_owned(),
            scope: None,
        }
    }

    fn id_token(email: &str, padding: usize) -> String {
        let claims = json!({"email": email, "pad": "x".repeat(padding)});
        format!(
            "header.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        )
    }

    #[test]
    fn credential_matches_the_apps_format_and_names_its_account() {
        let bytes = app_credential(&tokens(Some(id_token("User@Example.com", 0)))).unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["token"]["refresh_token"], "1//refresh");
        assert_eq!(value["token"]["token_type"], "Bearer");
        assert_eq!(value["token"]["expiry"], "2026-10-01T07:44:19.000000Z");
        assert_eq!(value["auth_method"], "consumer");
        assert_eq!(
            app_credential_email(&bytes).as_deref(),
            Some("user@example.com")
        );
    }

    #[test]
    fn oversized_id_token_is_dropped_and_missing_refresh_token_is_rejected() {
        let bytes = app_credential(&tokens(Some(id_token("a@example.com", 4000)))).unwrap();
        assert!(bytes.len() <= MAX_CREDENTIAL_BYTES);
        assert!(app_credential_email(&bytes).is_none());

        let mut without_refresh = tokens(None);
        without_refresh.refresh_token = None;
        assert!(app_credential(&without_refresh).is_err());
    }
}
