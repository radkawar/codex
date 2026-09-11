use std::io;
use std::path::Path;
use std::path::PathBuf;

use codex_app_server_protocol::Account;
use codex_app_server_protocol::AuthMode;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthKeyringBackendKind;
use codex_login::auth::AgentIdentityStorage;
use codex_login::load_auth_dot_json;
use codex_protocol::account::PlanType as AccountPlanType;
use codex_protocol::auth::AuthMode as CoreAuthMode;

const AUTH_PROFILES_DIRNAME: &str = "accounts";

pub(crate) fn load_auth_profile(codex_home: &Path, name: &str) -> io::Result<AuthDotJson> {
    validate_profile_name(name)?;
    let profile_home = auth_profile_home(codex_home, name);
    load_auth_dot_json(
        &profile_home,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("auth profile {name:?} not found"),
        )
    })
}

fn auth_profiles_dir(codex_home: &Path) -> PathBuf {
    codex_home.join(AUTH_PROFILES_DIRNAME)
}

fn auth_profile_home(codex_home: &Path, name: &str) -> PathBuf {
    auth_profiles_dir(codex_home).join(name)
}

pub(crate) fn account_from_auth(auth: &AuthDotJson) -> Option<Account> {
    match resolved_auth_mode(auth) {
        CoreAuthMode::ApiKey => Some(Account::ApiKey {}),
        CoreAuthMode::Chatgpt
        | CoreAuthMode::ChatgptAuthTokens
        | CoreAuthMode::PersonalAccessToken => {
            let tokens = auth.tokens.as_ref()?;
            let email = tokens.id_token.email.clone();
            let plan_type =
                account_plan_type_from_auth(tokens.id_token.get_chatgpt_plan_type_raw());
            Some(Account::Chatgpt { email, plan_type })
        }
        CoreAuthMode::AgentIdentity => {
            let AgentIdentityStorage::Record(record) = auth.agent_identity.as_ref()? else {
                return None;
            };
            Some(Account::Chatgpt {
                email: record.email.clone(),
                plan_type: record.plan_type,
            })
        }
        CoreAuthMode::BedrockApiKey | CoreAuthMode::BedrockAccessKeys => {
            Some(Account::AmazonBedrock {
                uses_codex_managed_credentials: true,
            })
        }
        CoreAuthMode::Headers => None,
    }
}

pub(crate) fn auth_mode_from_auth(auth: &AuthDotJson) -> AuthMode {
    match resolved_auth_mode(auth) {
        CoreAuthMode::ApiKey => AuthMode::ApiKey,
        CoreAuthMode::Chatgpt => AuthMode::Chatgpt,
        CoreAuthMode::ChatgptAuthTokens => AuthMode::ChatgptAuthTokens,
        CoreAuthMode::Headers => AuthMode::Headers,
        CoreAuthMode::AgentIdentity => AuthMode::AgentIdentity,
        CoreAuthMode::PersonalAccessToken => AuthMode::PersonalAccessToken,
        CoreAuthMode::BedrockApiKey => AuthMode::BedrockApiKey,
        CoreAuthMode::BedrockAccessKeys => AuthMode::BedrockAccessKeys,
    }
}

fn account_plan_type_from_auth(raw: Option<String>) -> AccountPlanType {
    raw.and_then(|value| serde_json::from_value::<AccountPlanType>(serde_json::json!(value)).ok())
        .unwrap_or(AccountPlanType::Unknown)
}

fn resolved_auth_mode(auth: &AuthDotJson) -> CoreAuthMode {
    if let Some(mode) = auth.auth_mode {
        return mode;
    }
    if auth.personal_access_token.is_some() {
        return CoreAuthMode::PersonalAccessToken;
    }
    if auth.bedrock_api_key.is_some() {
        return CoreAuthMode::BedrockApiKey;
    }
    if auth.bedrock_access_keys.is_some() {
        return CoreAuthMode::BedrockAccessKeys;
    }
    if auth.openai_api_key.is_some() {
        return CoreAuthMode::ApiKey;
    }
    CoreAuthMode::Chatgpt
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
