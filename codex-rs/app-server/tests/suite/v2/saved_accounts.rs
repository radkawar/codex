use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::TestAppServer;
use app_test_support::write_chatgpt_auth;
use codex_app_server_protocol::AccountSessionsResponse;
use codex_app_server_protocol::AuthProfileActivateNextResponse;
use codex_app_server_protocol::AuthProfileListResponse;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::load_auth_dot_json;
use codex_login::save_auth;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const FIRST_ID: &str = "00000000-0000-7000-8000-000000000001";
const SECOND_ID: &str = "00000000-0000-7000-8000-000000000002";
const READ_TIMEOUT: Duration = Duration::from_secs(60);

fn write_account(home: &Path, identity: &str) -> Result<()> {
    std::fs::create_dir_all(home)?;
    write_chatgpt_auth(
        home,
        ChatGptAuthFixture::new(format!("access-{identity}"))
            .refresh_token(format!("refresh-{identity}"))
            .account_id(format!("workspace-{identity}"))
            .chatgpt_user_id(format!("user-{identity}"))
            .email(format!("{identity}@example.com"))
            .plan_type("pro"),
        AuthCredentialsStoreMode::File,
    )?;
    // Browser OAuth may save an exchanged API key alongside managed ChatGPT tokens.
    let mut auth = load_auth_dot_json(
        home,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?
    .expect("saved fixture auth");
    auth.openai_api_key = Some(format!("companion-{identity}"));
    save_auth(
        home,
        &auth,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    Ok(())
}

async fn setup_accounts() -> Result<(TempDir, MockServer, TestAppServer)> {
    let home = TempDir::new()?;
    let backend = MockServer::start().await;
    let url = backend.uri();
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            r#"
chatgpt_base_url = "{url}"
cli_auth_credentials_store = "file"
model = "mock-model"
model_provider = "mock"
[features]
shell_snapshot = false
[model_providers.mock]
name = "Mock"
base_url = "{url}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )?;
    write_account(home.path(), "first")?;
    for (id, identity) in [(FIRST_ID, "first"), (SECOND_ID, "second")] {
        write_account(&home.path().join("account-sessions").join(id), identity)?;
    }
    let sessions = [(FIRST_ID, "first"), (SECOND_ID, "second")].map(|(id, identity)| json!({
            "sessionId": id, "email": format!("{identity}@example.com"),
            "userId": format!("user-{identity}"), "displayName": null, "imageUrl": null,
            "lastUsedAt": 1, "selectedWorkspaceAccountId": format!("workspace-{identity}"), "workspaces": []
        }));
    std::fs::write(
        home.path().join("account-sessions.json"),
        serde_json::to_vec(&json!({
            "schemaVersion": 1, "activeSessionId": FIRST_ID,
            "sessions": sessions,
        }))?,
    )?;
    for (identity, used) in [("first", 100), ("second", 25)] {
        Mock::given(method("GET")).and(path("/api/codex/usage"))
            .and(header("Authorization", format!("Bearer access-{identity}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plan_type": "pro", "rate_limit": {
                    "allowed": used < 100, "limit_reached": used == 100,
                    "primary_window": {"used_percent": used, "limit_window_seconds": 18000, "reset_after_seconds": 500, "reset_at": 2000000000},
                    "secondary_window": {"used_percent": used, "limit_window_seconds": 604800, "reset_after_seconds": 1000, "reset_at": 2000500000}
                }
            }))).mount(&backend).await;
    }
    let server = TestAppServer::builder()
        .with_codex_home(home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;
    Ok((home, backend, server))
}

#[tokio::test]
async fn canonical_accounts_supply_usage_and_automatic_rotation_without_named_profiles()
-> Result<()> {
    let (home, _backend, mut server) = setup_accounts().await?;
    let request_id = server
        .send_raw_request("accountSession/list", Some(json!({})))
        .await?;
    let response: AccountSessionsResponse =
        timeout(READ_TIMEOUT, server.read_response(request_id)).await??;
    assert_eq!(
        response
            .sessions
            .iter()
            .map(|session| (
                session.session_id.as_str(),
                session.is_active,
                session
                    .rate_limits
                    .as_ref()
                    .and_then(|limit| limit.primary.as_ref())
                    .map(|window| window.used_percent),
                session
                    .rate_limits
                    .as_ref()
                    .and_then(|limit| limit.secondary.as_ref())
                    .and_then(|window| window.resets_at),
            ))
            .collect::<Vec<_>>(),
        vec![
            (FIRST_ID, true, Some(100), Some(2000500000)),
            (SECOND_ID, false, Some(25), Some(2000500000))
        ]
    );

    let request_id = server
        .send_raw_request("account/authProfile/list", None)
        .await?;
    let compatibility: AuthProfileListResponse =
        timeout(READ_TIMEOUT, server.read_response(request_id)).await??;
    assert_eq!(
        compatibility
            .profiles
            .iter()
            .map(|profile| profile.name.as_str())
            .collect::<Vec<_>>(),
        vec![FIRST_ID, SECOND_ID]
    );
    let request_id = server
        .send_raw_request("account/authProfile/activateNext", None)
        .await?;
    let rotated: AuthProfileActivateNextResponse =
        timeout(READ_TIMEOUT, server.read_response(request_id)).await??;
    assert_eq!(
        (rotated.profile.name.as_str(), rotated.profile.active),
        (SECOND_ID, true)
    );
    let active = load_auth_dot_json(
        home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?
    .unwrap();
    assert_eq!(
        active.tokens.unwrap().id_token.email.as_deref(),
        Some("second@example.com")
    );
    assert!(!home.path().join("accounts").exists());
    Ok(())
}

#[tokio::test]
async fn email_switch_and_logout_use_the_same_saved_accounts() -> Result<()> {
    let (home, _backend, mut server) = setup_accounts().await?;
    let request_id = server
        .send_raw_request(
            "accountSession/switch",
            Some(json!({"sessionId": "SECOND@example.com"})),
        )
        .await?;
    let switched: AccountSessionsResponse =
        timeout(READ_TIMEOUT, server.read_response(request_id)).await??;
    assert_eq!(switched.active_session_id.as_deref(), Some(SECOND_ID));
    let request_id = server
        .send_raw_request(
            "accountSession/logout",
            Some(json!({"sessionId": "first@example.com"})),
        )
        .await?;
    let remaining: AccountSessionsResponse =
        timeout(READ_TIMEOUT, server.read_response(request_id)).await??;
    assert_eq!(
        remaining
            .sessions
            .iter()
            .map(|session| session.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![SECOND_ID]
    );
    assert!(
        !home
            .path()
            .join("account-sessions")
            .join(FIRST_ID)
            .join("auth.json")
            .exists()
    );
    Ok(())
}
