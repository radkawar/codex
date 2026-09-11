use super::*;
use anyhow::Result;
use app_test_support::ChatGptIdTokenClaims;
use app_test_support::encode_id_token;
use codex_app_server_protocol::Account;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_protocol::auth::AuthMode;
use core_test_support::load_default_config_for_test;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use tempfile::TempDir;

async fn test_config(home: &TempDir) -> (Config, Arc<AuthManager>) {
    let mut config = load_default_config_for_test(home).await;
    config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
    config.chatgpt_base_url = "http://127.0.0.1:0".to_string();
    let manager = AuthManager::shared(
        home.path().to_path_buf(),
        /*enable_codex_api_key_env*/ false,
        AuthCredentialsStoreMode::File,
        /*forced_chatgpt_workspace_id*/ None,
        Some(config.chatgpt_base_url.clone()),
        config.auth_keyring_backend_kind(),
        config.auth_route_config(),
    )
    .await;
    (config, manager)
}

fn chatgpt_auth(user_id: &str, email: &str, access_token: &str) -> Result<AuthDotJson> {
    let id_token = encode_id_token(&ChatGptIdTokenClaims {
        email: Some(email.to_string()),
        chatgpt_user_id: Some(user_id.to_string()),
        chatgpt_account_id: Some("shared-workspace".to_string()),
        ..Default::default()
    })?;
    Ok(serde_json::from_value(json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": access_token,
            "refresh_token": "",
            "account_id": "shared-workspace"
        },
        "last_refresh": Utc::now()
    }))?)
}

fn write_auth(home: &Path, auth: &AuthDotJson) -> Result<()> {
    std::fs::create_dir_all(home)?;
    save_auth(
        home,
        auth,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    Ok(())
}

#[tokio::test]
async fn migration_preserves_canonical_credentials_and_deduplicates_legacy_identities() -> Result<()>
{
    let home = TempDir::new()?;
    let (config, manager) = test_config(&home).await;
    let store = AccountSessionsStore::new(&config, &manager);
    let canonical_auth = chatgpt_auth("user-1", "shared@example.com", "canonical")?;
    let canonical = AccountSessionsStore::session_from_auth_json(&canonical_auth);
    let canonical_id = canonical.session_id.clone();
    store.save_session_auth(&canonical_id, &canonical_auth)?;
    store.save_unlocked(&StoredAccountSessions {
        sessions: vec![canonical],
        ..Default::default()
    })?;

    let mut stale = canonical_auth.clone();
    stale.tokens.as_mut().unwrap().access_token = "stale-legacy".to_string();
    // A legacy timestamp cannot override a canonical slot, even if it is newer.
    stale.last_refresh = Some(Utc::now() + chrono::Duration::hours(1));
    write_auth(&home.path().join("accounts/old-name"), &stale)?;
    let newest = chatgpt_auth("user-2", "shared@example.com", "newest-legacy")?;
    let mut older = newest.clone();
    older.tokens.as_mut().unwrap().access_token = "older-legacy".to_string();
    older.last_refresh = Some(Utc::now() - chrono::Duration::days(1));
    write_auth(&home.path().join("accounts/a-old"), &older)?;
    write_auth(&home.path().join("accounts/z-new"), &newest)?;

    let listed = store.list(/*refresh_workspace_metadata*/ false).await?;
    assert_eq!(listed.sessions.len(), 2);
    assert_eq!(
        store.load_session_auth(&canonical_id)?,
        Some(canonical_auth)
    );
    let imported = listed
        .sessions
        .iter()
        .find(|session| session.session_id != canonical_id)
        .unwrap();
    assert_eq!(store.load_session_auth(&imported.session_id)?, Some(newest));
    assert_eq!(
        store
            .switch("SHARED@example.com", /*account_id*/ None)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    assert_eq!(store.load_active_auth()?, None);
    assert_eq!(
        store.list(/*refresh_workspace_metadata*/ false).await?,
        listed
    );
    Ok(())
}

#[tokio::test]
async fn migrated_api_key_and_bedrock_sessions_switch_without_workspace_exchange() -> Result<()> {
    let home = TempDir::new()?;
    let api_key: AuthDotJson =
        serde_json::from_value(json!({"auth_mode": "apikey", "OPENAI_API_KEY": "api-key"}))?;
    let bedrock: AuthDotJson = serde_json::from_value(json!({
        "auth_mode": "bedrockAccessKeys",
        "bedrock_access_keys": {
            "access_key_id": "access",
            "secret_access_key": "secret",
            "session_token": "session"
        }
    }))?;
    write_auth(home.path(), &api_key)?;
    let mut duplicate_api_key = api_key.clone();
    duplicate_api_key.auth_mode = None;
    duplicate_api_key.last_refresh = Some(Utc::now());
    write_auth(&home.path().join("accounts/key"), &duplicate_api_key)?;
    write_auth(&home.path().join("accounts/bedrock"), &bedrock)?;
    let (config, manager) = test_config(&home).await;
    let store = AccountSessionsStore::new(&config, &manager);
    let listed = store.list(/*refresh_workspace_metadata*/ false).await?;
    assert_eq!(listed.sessions.len(), 2);
    let saved_bedrock = listed
        .sessions
        .iter()
        .find(|session| {
            session.account
                == Some(Account::AmazonBedrock {
                    uses_codex_managed_credentials: true,
                })
        })
        .unwrap();
    store
        .switch(&saved_bedrock.session_id, /*account_id*/ None)
        .await?;
    assert_eq!(store.load_active_auth()?, Some(bedrock));
    assert!(
        store
            .switch(&saved_bedrock.session_id, Some("workspace"))
            .await
            .is_err()
    );
    let saved_key = listed
        .sessions
        .iter()
        .find(|session| session.is_active)
        .unwrap();
    store
        .switch(&saved_key.session_id, /*account_id*/ None)
        .await?;
    assert_eq!(store.load_active_auth()?, Some(api_key));
    Ok(())
}

#[tokio::test]
async fn logout_retains_migration_marker_and_does_not_restore_legacy_profiles() -> Result<()> {
    let home = TempDir::new()?;
    let auth: AuthDotJson = serde_json::from_value(json!({"OPENAI_API_KEY": "api-key"}))?;
    write_auth(&home.path().join("accounts/legacy"), &auth)?;
    let (config, manager) = test_config(&home).await;
    let store = AccountSessionsStore::new(&config, &manager);
    assert_eq!(store.revoke_all_and_clear().await?, true);
    assert_eq!(
        store.list(/*refresh_workspace_metadata*/ false).await?,
        AccountSessionsResponse {
            active_session_id: None,
            sessions: Vec::new()
        }
    );
    assert!(home.path().join("accounts/legacy/auth.json").exists());
    assert!(store.read_unlocked()?.unwrap().legacy_profiles_migrated);
    Ok(())
}

#[tokio::test]
async fn conditional_refresh_rejects_changed_and_deleted_sessions_without_switching_active_auth()
-> Result<()> {
    let home = TempDir::new()?;
    let original = chatgpt_auth("user-1", "first@example.com", "original")?;
    let other = chatgpt_auth("user-2", "second@example.com", "other")?;
    write_auth(home.path(), &original)?;
    write_auth(&home.path().join("accounts/other"), &other)?;
    let (config, manager) = test_config(&home).await;
    let store = AccountSessionsStore::new(&config, &manager);
    let snapshot = store
        .snapshot_session_credentials("second@example.com")
        .await?;
    let mut refreshed = other.clone();
    refreshed.tokens.as_mut().unwrap().access_token = "refreshed".to_string();
    assert!(
        store
            .persist_session_credentials_if_unchanged(&snapshot, &refreshed)
            .await?
    );
    assert_eq!(store.load_active_auth()?, Some(original.clone()));
    assert!(
        !store
            .persist_session_credentials_if_unchanged(&snapshot, &other)
            .await?
    );
    assert_eq!(
        store.load_session_auth(&snapshot.session_id)?,
        Some(refreshed)
    );

    let active_snapshot = store
        .snapshot_session_credentials("first@example.com")
        .await?;
    let mut active_refresh = original.clone();
    active_refresh.tokens.as_mut().unwrap().access_token = "active-refreshed".to_string();
    assert!(
        store
            .persist_session_credentials_if_unchanged(&active_snapshot, &active_refresh)
            .await?
    );
    assert_eq!(store.load_active_auth()?, Some(active_refresh.clone()));
    assert_eq!(
        manager
            .auth_cached()
            .unwrap()
            .get_token_data()?
            .access_token,
        "active-refreshed"
    );

    let deletion_snapshot = store
        .snapshot_session_credentials(&snapshot.session_id)
        .await?;
    // Delete through the store without contacting OAuth for this synthetic token.
    let mut nonrevocable = deletion_snapshot.auth.clone();
    nonrevocable.tokens.as_mut().unwrap().access_token.clear();
    store.save_session_auth(&snapshot.session_id, &nonrevocable)?;
    store.logout(&snapshot.session_id).await?;
    assert!(
        !store
            .persist_session_credentials_if_unchanged(&deletion_snapshot, &other)
            .await?
    );
    assert_eq!(store.load_session_auth(&snapshot.session_id)?, None);
    assert_eq!(store.load_active_auth()?, Some(active_refresh));
    Ok(())
}

#[tokio::test]
async fn add_without_switching_restores_previously_active_credentials() -> Result<()> {
    let home = TempDir::new()?;
    let original = chatgpt_auth("user-1", "first@example.com", "original")?;
    let added = chatgpt_auth("user-2", "second@example.com", "added")?;
    write_auth(home.path(), &original)?;
    let (config, manager) = test_config(&home).await;
    let store = AccountSessionsStore::new(&config, &manager);
    let before = store.list(/*refresh_workspace_metadata*/ false).await?;
    write_auth(home.path(), &added)?;
    manager.reload().await;
    let after = store.add(/*switch_to_added_account*/ false).await?;
    assert_eq!(after.active_session_id, before.active_session_id);
    assert_eq!(after.sessions.len(), 2);
    assert_eq!(store.load_active_auth()?, Some(original));
    Ok(())
}

#[tokio::test]
async fn external_token_switch_clears_inactive_stores_and_can_switch_back() -> Result<()> {
    let home = TempDir::new()?;
    let original: AuthDotJson = serde_json::from_value(json!({"OPENAI_API_KEY": "api-key"}))?;
    let mut external = chatgpt_auth("user-1", "external@example.com", "external")?;
    external.auth_mode = Some(AuthMode::ChatgptAuthTokens);
    write_auth(home.path(), &original)?;
    write_auth(&home.path().join("accounts/external"), &external)?;
    let (config, manager) = test_config(&home).await;
    let store = AccountSessionsStore::new(&config, &manager);
    let before = store.list(/*refresh_workspace_metadata*/ false).await?;
    store
        .switch("external@example.com", /*account_id*/ None)
        .await?;
    manager.reload().await;
    assert_eq!(store.load_active_auth()?, Some(external.clone()));
    assert!(!home.path().join("auth.json").exists());
    assert_eq!(
        manager.auth_cached().unwrap().api_auth_mode(),
        AuthMode::ChatgptAuthTokens
    );
    let snapshot = store
        .snapshot_session_credentials("external@example.com")
        .await?;
    assert_eq!(snapshot.auth, external);
    store
        .switch(
            before.active_session_id.as_deref().unwrap(),
            /*account_id*/ None,
        )
        .await?;
    manager.reload().await;
    assert_eq!(store.load_active_auth()?, Some(original));
    assert_eq!(
        load_auth_dot_json(
            home.path(),
            AuthCredentialsStoreMode::Ephemeral,
            AuthKeyringBackendKind::default()
        )?,
        None
    );
    assert_eq!(manager.auth_cached().unwrap().auth_mode(), AuthMode::ApiKey);
    Ok(())
}

#[tokio::test]
async fn corrupt_legacy_profile_does_not_block_migration_or_return_after_logout() -> Result<()> {
    let home = TempDir::new()?;
    let auth: AuthDotJson = serde_json::from_value(json!({"OPENAI_API_KEY": "valid"}))?;
    write_auth(&home.path().join("accounts/valid"), &auth)?;
    let corrupt_home = home.path().join("accounts/corrupt");
    std::fs::create_dir_all(&corrupt_home)?;
    std::fs::write(corrupt_home.join("auth.json"), "{")?;
    let (config, manager) = test_config(&home).await;
    let store = AccountSessionsStore::new(&config, &manager);
    let listed = store.list(/*refresh_workspace_metadata*/ false).await?;
    assert_eq!(listed.sessions.len(), 1);
    assert_eq!(
        store.load_session_auth(&listed.sessions[0].session_id)?,
        Some(auth.clone())
    );
    assert_eq!(
        std::fs::read_to_string(corrupt_home.join("auth.json"))?,
        "{"
    );
    store.logout(&listed.sessions[0].session_id).await?;
    // A later repair of the archived source must not restore a logged-out account.
    write_auth(&corrupt_home, &auth)?;
    assert_eq!(
        store.list(/*refresh_workspace_metadata*/ false).await?,
        AccountSessionsResponse {
            active_session_id: None,
            sessions: Vec::new()
        }
    );
    Ok(())
}

#[tokio::test]
async fn invalid_session_ids_are_rejected_before_paths_are_used_and_logout_can_clear_corrupt_index()
-> Result<()> {
    let home = TempDir::new()?;
    let auth: AuthDotJson = serde_json::from_value(json!({"OPENAI_API_KEY": "original"}))?;
    write_auth(home.path(), &auth)?;
    let (config, manager) = test_config(&home).await;
    let store = AccountSessionsStore::new(&config, &manager);
    let mut session = AccountSessionsStore::session_from_auth_json(&auth);
    session.session_id = "../outside".to_string();
    store.save_unlocked(&StoredAccountSessions {
        sessions: vec![session],
        ..Default::default()
    })?;
    assert_eq!(
        store
            .list(/*refresh_workspace_metadata*/ false)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(store.load_active_auth()?, Some(auth));

    std::fs::write(store.path(), "{")?;
    assert!(!store.revoke_all_and_clear().await?);
    manager.logout().await?;
    assert_eq!(
        store.list(/*refresh_workspace_metadata*/ false).await?,
        AccountSessionsResponse {
            active_session_id: None,
            sessions: Vec::new()
        }
    );
    Ok(())
}

#[tokio::test]
async fn switch_rejects_disallowed_login_without_replacing_active_credentials() -> Result<()> {
    let home = TempDir::new()?;
    let original: AuthDotJson = serde_json::from_value(json!({"OPENAI_API_KEY": "original"}))?;
    write_auth(home.path(), &original)?;
    let managed = chatgpt_auth("user-1", "saved@example.com", "saved")?;
    write_auth(&home.path().join("accounts/saved"), &managed)?;
    let (mut config, manager) = test_config(&home).await;
    config.forced_login_method = Some(codex_protocol::config_types::ForcedLoginMethod::Api);
    let store = AccountSessionsStore::new(&config, &manager);
    let before = store.list(/*refresh_workspace_metadata*/ false).await?;
    assert_eq!(
        store
            .switch("saved@example.com", /*account_id*/ None)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::PermissionDenied
    );
    assert_eq!(store.load_active_auth()?, Some(original));
    assert_eq!(
        store.list(/*refresh_workspace_metadata*/ false).await?,
        before
    );
    Ok(())
}
