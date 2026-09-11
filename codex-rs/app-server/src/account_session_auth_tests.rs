use super::*;
use app_test_support::ChatGptAuthFixture;
use app_test_support::write_chatgpt_auth;
use codex_core::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use std::time::Duration;

#[tokio::test]
async fn concurrent_readers_snapshot_after_previous_refresh_is_committed() -> anyhow::Result<()> {
    let home = TempDir::new()?;
    write_chatgpt_auth(
        home.path(),
        ChatGptAuthFixture::new("original")
            .email("saved@example.com")
            .chatgpt_account_id("workspace"),
        AuthCredentialsStoreMode::File,
    )?;
    let mut config = ConfigBuilder::default()
        .codex_home(home.path().to_path_buf())
        .build()
        .await?;
    config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
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
    let store = AccountSessionsStore::new(&config, &manager);
    let accounts = store.list(/*refresh_workspace_metadata*/ false).await?;
    let id = &accounts.sessions[0].session_id;
    let first = IsolatedAccountSessionAuth::new(&config, &store, id).await?;
    let mut refreshed = first.initial_auth().clone();
    refreshed.tokens.as_mut().unwrap().access_token = "rotated".to_string();
    save_auth(
        first.credentials.home.path(),
        &refreshed,
        first.credentials.store_mode,
        first.credentials.keyring_backend,
    )?;
    let second = IsolatedAccountSessionAuth::new(&config, &store, id);
    tokio::pin!(second);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut second)
            .await
            .is_err()
    );
    assert!(first.persist_if_changed(&store).await?);
    drop(first);
    let second = second.await?;
    assert_eq!(second.initial_auth(), &refreshed);
    assert_eq!(
        load_auth_dot_json(
            home.path(),
            AuthCredentialsStoreMode::File,
            config.auth_keyring_backend_kind()
        )?,
        Some(refreshed)
    );
    Ok(())
}
