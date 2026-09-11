use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::TestAppServer;
use app_test_support::write_chatgpt_auth;
use codex_app_server_protocol::Account;
use codex_app_server_protocol::AccountSession;
use codex_app_server_protocol::AccountSessionWorkspace;
use codex_app_server_protocol::AccountSessionWorkspaceKind;
use codex_app_server_protocol::AccountSessionsResponse;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::load_auth_dot_json;
use codex_protocol::account::PlanType;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_json;
use wiremock::matchers::method;
use wiremock::matchers::path;

const READ_TIMEOUT: Duration = Duration::from_secs(60);
const ORIGINAL_WORKSPACE_ID: &str = "workspace-original";
const NEW_WORKSPACE_ID: &str = "workspace-new";

fn write_config(codex_home: &Path, chatgpt_base_url: &str) -> std::io::Result<()> {
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            r#"
model = "mock-model"
approval_policy = "never"
sandbox_mode = "danger-full-access"
chatgpt_base_url = "{chatgpt_base_url}"

model_provider = "mock"

[features]
shell_snapshot = false

[model_providers.mock]
name = "Mock"
base_url = "http://127.0.0.1:0/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )
}

fn write_initial_auth(codex_home: &Path) -> Result<()> {
    write_chatgpt_auth(
        codex_home,
        ChatGptAuthFixture::new("access-token-original")
            .refresh_token("refresh-token-original")
            .account_id(ORIGINAL_WORKSPACE_ID)
            .chatgpt_account_id(ORIGINAL_WORKSPACE_ID)
            .chatgpt_user_id("user-1")
            .email("person@example.com"),
        AuthCredentialsStoreMode::File,
    )
}

async fn list_sessions(mcp: &mut TestAppServer) -> Result<AccountSessionsResponse> {
    let request_id = mcp
        .send_raw_request(
            "accountSession/list",
            Some(json!({ "refreshWorkspaceMetadata": false })),
        )
        .await?;
    timeout(READ_TIMEOUT, mcp.read_response(request_id)).await?
}

#[tokio::test]
async fn list_imports_managed_auth_without_putting_tokens_in_metadata() -> Result<()> {
    let codex_home = TempDir::new()?;
    write_config(codex_home.path(), "http://127.0.0.1:0")?;
    write_initial_auth(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;
    let response = list_sessions(&mut mcp).await?;
    let session = response.sessions.first().expect("one imported session");
    assert_eq!(
        response,
        AccountSessionsResponse {
            active_session_id: Some(session.session_id.clone()),
            sessions: vec![AccountSession {
                session_id: session.session_id.clone(),
                account: Some(Account::Chatgpt {
                    email: Some("person@example.com".to_string()),
                    plan_type: PlanType::Unknown,
                }),
                rate_limits: None,
                email: Some("person@example.com".to_string()),
                user_id: Some("user-1".to_string()),
                display_name: None,
                image_url: None,
                last_used_at: session.last_used_at,
                is_active: true,
                selected_workspace_account_id: Some(ORIGINAL_WORKSPACE_ID.to_string()),
                workspaces: vec![AccountSessionWorkspace {
                    account_id: ORIGINAL_WORKSPACE_ID.to_string(),
                    name: None,
                    image_url: None,
                    kind: None,
                }],
            }],
        }
    );

    let metadata = std::fs::read_to_string(codex_home.path().join("account-sessions.json"))?;
    assert!(!metadata.contains("access-token-original"));
    assert!(!metadata.contains("refresh-token-original"));
    let metadata: serde_json::Value = serde_json::from_str(&metadata)?;
    assert!(metadata["sessions"][0].get("authJson").is_none());

    let slot_auth = load_auth_dot_json(
        &codex_home
            .path()
            .join("account-sessions")
            .join(&session.session_id),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?
    .expect("credential slot");
    assert_eq!(
        slot_auth
            .tokens
            .expect("managed ChatGPT tokens")
            .access_token,
        "access-token-original"
    );

    let request_id = mcp
        .send_raw_request(
            "accountSession/switch",
            Some(json!({ "sessionId": session.session_id.clone() })),
        )
        .await?;
    let switched: AccountSessionsResponse =
        timeout(READ_TIMEOUT, mcp.read_response(request_id)).await??;
    assert_eq!(switched, response);
    Ok(())
}

#[tokio::test]
async fn switch_exchanges_workspace_token_and_updates_active_auth() -> Result<()> {
    let codex_home = TempDir::new()?;
    let backend = MockServer::start().await;
    write_config(codex_home.path(), &backend.uri())?;
    write_initial_auth(codex_home.path())?;

    Mock::given(method("GET"))
        .and(path("/api/codex/accounts/check"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "accounts": [
                {
                    "id": ORIGINAL_WORKSPACE_ID,
                    "name": "Original",
                    "structure": "personal"
                },
                {
                    "id": NEW_WORKSPACE_ID,
                    "name": "New workspace",
                    "structure": "workspace"
                }
            ],
            "account_ordering": [ORIGINAL_WORKSPACE_ID, NEW_WORKSPACE_ID],
            "default_account_id": ORIGINAL_WORKSPACE_ID
        })))
        .expect(1)
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path("/accounts/switch-workspace-token"))
        .and(body_json(json!({ "workspace_id": NEW_WORKSPACE_ID })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "access-token-new",
            "refresh_token": "refresh-token-new"
        })))
        .expect(1)
        .mount(&backend)
        .await;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;
    let listed = list_sessions(&mut mcp).await?;
    let session_id = listed.sessions[0].session_id.clone();
    let request_id = mcp
        .send_raw_request(
            "accountSession/switch",
            Some(json!({
                "sessionId": session_id,
                "accountId": NEW_WORKSPACE_ID
            })),
        )
        .await?;
    let response: AccountSessionsResponse =
        timeout(READ_TIMEOUT, mcp.read_response(request_id)).await??;
    let switched = &response.sessions[0];
    assert_eq!(
        response,
        AccountSessionsResponse {
            active_session_id: Some(session_id.clone()),
            sessions: vec![AccountSession {
                session_id,
                account: Some(Account::Chatgpt {
                    email: Some("person@example.com".to_string()),
                    plan_type: PlanType::Unknown,
                }),
                rate_limits: None,
                email: Some("person@example.com".to_string()),
                user_id: Some("user-1".to_string()),
                display_name: None,
                image_url: None,
                last_used_at: switched.last_used_at,
                is_active: true,
                selected_workspace_account_id: Some(NEW_WORKSPACE_ID.to_string()),
                workspaces: vec![
                    AccountSessionWorkspace {
                        account_id: ORIGINAL_WORKSPACE_ID.to_string(),
                        name: Some("Original".to_string()),
                        image_url: None,
                        kind: Some(AccountSessionWorkspaceKind::Personal),
                    },
                    AccountSessionWorkspace {
                        account_id: NEW_WORKSPACE_ID.to_string(),
                        name: Some("New workspace".to_string()),
                        image_url: None,
                        kind: Some(AccountSessionWorkspaceKind::Workspace),
                    },
                ],
            }],
        }
    );

    let active = load_auth_dot_json(
        codex_home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?
    .expect("active auth");
    let tokens = active.tokens.expect("managed ChatGPT tokens");
    assert_eq!(tokens.access_token, "access-token-new");
    assert_eq!(tokens.refresh_token, "refresh-token-new");
    assert_eq!(tokens.account_id.as_deref(), Some(NEW_WORKSPACE_ID));
    Ok(())
}
