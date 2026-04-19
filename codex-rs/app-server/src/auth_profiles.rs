use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::Account;
use codex_app_server_protocol::AuthMode;
use codex_app_server_protocol::AuthProfileSummary;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::load_auth_dot_json;
use codex_login::save_auth;
use codex_protocol::account::PlanType as AccountPlanType;

const AUTH_PROFILES_DIRNAME: &str = "accounts";

pub(crate) fn list_auth_profiles(
    codex_home: &Path,
    current_auth: Option<&AuthDotJson>,
) -> io::Result<Vec<AuthProfileSummary>> {
    let profiles_dir = auth_profiles_dir(codex_home);
    if !profiles_dir.exists() {
        return Ok(Vec::new());
    }

    let mut profiles = Vec::new();
    for entry in fs::read_dir(profiles_dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }

        let name = entry.file_name().to_string_lossy().to_string();
        if !is_valid_profile_name(&name) {
            continue;
        }

        let profile_home = entry.path();
        let Some(auth) = load_auth_dot_json(&profile_home, AuthCredentialsStoreMode::File)? else {
            continue;
        };

        profiles.push(summary_from_auth(name, &auth, current_auth));
    }

    profiles.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(profiles)
}

pub(crate) fn save_auth_profile(
    codex_home: &Path,
    name: &str,
    auth: &AuthDotJson,
    overwrite: bool,
    current_auth: Option<&AuthDotJson>,
) -> io::Result<AuthProfileSummary> {
    validate_profile_name(name)?;
    let profile_home = auth_profile_home(codex_home, name);
    if profile_home.exists() && !overwrite {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("auth profile {name:?} already exists"),
        ));
    }

    fs::create_dir_all(&profile_home)?;
    save_auth(&profile_home, auth, AuthCredentialsStoreMode::File)?;
    Ok(summary_from_auth(
        name.to_string(),
        auth,
        current_auth.or(Some(auth)),
    ))
}

pub(crate) fn load_auth_profile(codex_home: &Path, name: &str) -> io::Result<AuthDotJson> {
    validate_profile_name(name)?;
    let profile_home = auth_profile_home(codex_home, name);
    load_auth_dot_json(&profile_home, AuthCredentialsStoreMode::File)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("auth profile {name:?} not found"),
        )
    })
}

pub(crate) fn delete_auth_profile(codex_home: &Path, name: &str) -> io::Result<bool> {
    validate_profile_name(name)?;
    let profile_home = auth_profile_home(codex_home, name);
    if !profile_home.exists() {
        return Ok(false);
    }
    fs::remove_dir_all(profile_home)?;
    Ok(true)
}

fn auth_profiles_dir(codex_home: &Path) -> PathBuf {
    codex_home.join(AUTH_PROFILES_DIRNAME)
}

fn auth_profile_home(codex_home: &Path, name: &str) -> PathBuf {
    auth_profiles_dir(codex_home).join(name)
}

fn summary_from_auth(
    name: String,
    auth: &AuthDotJson,
    current_auth: Option<&AuthDotJson>,
) -> AuthProfileSummary {
    AuthProfileSummary {
        name,
        account: account_from_auth(auth),
        rate_limits: None,
        active: current_auth.is_some_and(|current| auth_profile_matches_current(current, auth)),
    }
}

pub(crate) fn account_from_auth(auth: &AuthDotJson) -> Option<Account> {
    match resolved_auth_mode(auth) {
        ResolvedAuthMode::ApiKey => Some(Account::ApiKey {}),
        ResolvedAuthMode::Chatgpt => {
            let tokens = auth.tokens.as_ref()?;
            let email = tokens.id_token.email.clone()?;
            let plan_type =
                account_plan_type_from_auth(tokens.id_token.get_chatgpt_plan_type_raw());
            Some(Account::Chatgpt { email, plan_type })
        }
    }
}

pub(crate) fn auth_mode_from_auth(auth: &AuthDotJson) -> AuthMode {
    if auth.openai_api_key.is_some() || matches!(auth.auth_mode, Some(AuthMode::ApiKey)) {
        AuthMode::ApiKey
    } else if matches!(auth.auth_mode, Some(AuthMode::ChatgptAuthTokens)) {
        AuthMode::ChatgptAuthTokens
    } else {
        match resolved_auth_mode(auth) {
            ResolvedAuthMode::ApiKey => AuthMode::ApiKey,
            ResolvedAuthMode::Chatgpt => AuthMode::Chatgpt,
        }
    }
}

fn account_plan_type_from_auth(raw: Option<String>) -> AccountPlanType {
    raw.and_then(|value| serde_json::from_value::<AccountPlanType>(serde_json::json!(value)).ok())
        .unwrap_or(AccountPlanType::Unknown)
}

fn auth_profile_matches_current(current: &AuthDotJson, candidate: &AuthDotJson) -> bool {
    match (resolved_auth_mode(current), resolved_auth_mode(candidate)) {
        (ResolvedAuthMode::ApiKey, ResolvedAuthMode::ApiKey) => {
            current.openai_api_key == candidate.openai_api_key
        }
        (ResolvedAuthMode::Chatgpt, ResolvedAuthMode::Chatgpt) => {
            auth_account_id(current) == auth_account_id(candidate)
                && auth_account_id(current).is_some()
        }
        _ => false,
    }
}

fn auth_account_id(auth: &AuthDotJson) -> Option<&str> {
    auth.tokens
        .as_ref()
        .and_then(|tokens| tokens.account_id.as_deref())
        .or_else(|| {
            auth.tokens
                .as_ref()
                .and_then(|tokens| tokens.id_token.chatgpt_account_id.as_deref())
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedAuthMode {
    ApiKey,
    Chatgpt,
}

fn resolved_auth_mode(auth: &AuthDotJson) -> ResolvedAuthMode {
    if auth.openai_api_key.is_some()
        || matches!(
            auth.auth_mode,
            Some(codex_app_server_protocol::AuthMode::ApiKey)
        )
    {
        ResolvedAuthMode::ApiKey
    } else {
        ResolvedAuthMode::Chatgpt
    }
}

fn validate_profile_name(name: &str) -> io::Result<()> {
    if is_valid_profile_name(name) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "profile name must use only letters, numbers, '.', '_' or '-'",
        ))
    }
}

fn is_valid_profile_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use codex_login::TokenData;
    use codex_login::token_data::parse_chatgpt_jwt_claims;
    use serde_json::json;
    use tempfile::tempdir;

    fn fake_jwt(payload: serde_json::Value) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&payload).expect("serialize payload"));
        format!("{header}.{payload}.signature")
    }

    fn chatgpt_auth(account_id: &str, email: &str) -> AuthDotJson {
        let id_token_raw = fake_jwt(json!({
            "email": email,
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "pro",
                "chatgpt_account_id": account_id,
            }
        }));
        let tokens = TokenData {
            id_token: parse_chatgpt_jwt_claims(&id_token_raw).expect("parse id token"),
            account_id: Some(account_id.to_string()),
            ..Default::default()
        };

        AuthDotJson {
            auth_mode: Some(codex_app_server_protocol::AuthMode::Chatgpt),
            openai_api_key: None,
            tokens: Some(tokens),
            last_refresh: None,
            agent_identity: None,
        }
    }

    #[test]
    fn save_and_list_profiles_uses_canonical_auth_json() {
        let dir = tempdir().expect("tempdir");
        let auth = chatgpt_auth("acct-1", "user@example.com");

        let saved = save_auth_profile(dir.path(), "work", &auth, false, Some(&auth))
            .expect("profile should save");
        assert!(saved.active);

        let stored = load_auth_dot_json(
            &dir.path().join(AUTH_PROFILES_DIRNAME).join("work"),
            AuthCredentialsStoreMode::File,
        )
        .expect("stored auth should load")
        .expect("stored auth should exist");
        assert_eq!(stored, auth);

        let profiles = list_auth_profiles(dir.path(), Some(&auth)).expect("profiles should list");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0], saved);
    }

    #[test]
    fn invalid_profile_names_are_rejected() {
        let dir = tempdir().expect("tempdir");
        let auth = chatgpt_auth("acct-1", "user@example.com");

        let err = save_auth_profile(dir.path(), "../bad", &auth, false, None)
            .expect_err("invalid profile name should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }
}
