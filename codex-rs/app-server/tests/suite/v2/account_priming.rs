use anyhow::Result;
use app_test_support::ChatGptAuthFixture;
use app_test_support::TestAppServer;
use app_test_support::write_chatgpt_auth;
use app_test_support::write_models_cache;
use codex_app_server_protocol::AccountPrimingProfileOutcome;
use codex_app_server_protocol::AccountPrimingReadResponse;
use codex_app_server_protocol::AccountPrimingRunOnceResponse;
use codex_app_server_protocol::AccountPrimingStartResponse;
use codex_app_server_protocol::AccountPrimingStopResponse;
use codex_app_server_protocol::RequestId;
use codex_config::types::AuthCredentialsStoreMode;
use codex_login::AuthKeyringBackendKind;
use codex_login::REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR;
use codex_login::load_auth_dot_json;
use codex_login::save_auth;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tempfile::TempDir;
use test_case::test_case;
use tokio::sync::Notify;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const READ_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
enum CurrentProvider {
    OpenAi,
    Other,
}

async fn setup(
    server: &MockServer,
    provider: CurrentProvider,
) -> Result<(TempDir, TestAppServer, MockServer)> {
    let home = TempDir::new()?;
    let url = server.uri();
    let refresh_url = format!("{url}/oauth/token");
    let other = MockServer::start().await;
    let other_url = other.uri();
    let provider_id = match provider {
        CurrentProvider::OpenAi => "openai",
        CurrentProvider::Other => "other",
    };
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&other)
        .await;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            r#"
chatgpt_base_url = "{url}"
openai_base_url = "{url}"
cli_auth_credentials_store = "file"
model = "mock-model"
developer_instructions = "PRIVATE_PROJECT_INSTRUCTIONS"
model_provider = "{provider_id}"
[features]
shell_snapshot = false
[model_providers.other]
name = "Unrelated"
base_url = "{other_url}"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
"#
        ),
    )?;
    write_chatgpt_auth(
        home.path(),
        ChatGptAuthFixture::new("saved-token")
            .account_id("workspace-saved")
            .chatgpt_account_id("workspace-saved")
            .email("saved@example.com")
            .plan_type("pro"),
        AuthCredentialsStoreMode::File,
    )?;
    let mut auth = load_auth_dot_json(
        home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?
    .expect("saved browser auth fixture");
    auth.openai_api_key = Some("browser-companion-key".to_string());
    save_auth(
        home.path(),
        &auth,
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?;
    write_models_cache(home.path()).await?;
    let app = TestAppServer::builder()
        .with_codex_home(home.path())
        .with_env_overrides(&[
            ("OPENAI_API_KEY", None),
            (
                REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR,
                Some(refresh_url.as_str()),
            ),
        ])
        .build_initialized_with_timeout(READ_TIMEOUT)
        .await?;
    Ok((home, app, other))
}

fn usage(active: bool) -> serde_json::Value {
    let window = |seconds| {
        json!({
            "used_percent": 1, "limit_window_seconds": seconds,
            "reset_after_seconds": 500, "reset_at": 2000000000,
        })
    };
    json!({"plan_type": "pro", "rate_limit": {
        "allowed": true, "limit_reached": false,
        "primary_window": active.then(|| window(18000)),
        "secondary_window": active.then(|| window(604800)),
    }})
}

#[tokio::test]
async fn priming_recovers_rejected_access_token_and_preserves_rotated_credentials() -> Result<()> {
    let backend = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .and(header("authorization", "Bearer saved-token"))
        .respond_with(ResponseTemplate::new(401))
        .expect(1)
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "renewed-token", "refresh_token": "renewed-refresh-token"
        })))
        .expect(1)
        .mount(&backend)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .and(header("authorization", "Bearer renewed-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(usage(true)))
        .expect(1)
        .mount(&backend)
        .await;
    let (home, mut app, _other) = setup(&backend, CurrentProvider::OpenAi).await?;
    let id = app.send_raw_request("accountPriming/runOnce", None).await?;
    let response: AccountPrimingRunOnceResponse =
        timeout(READ_TIMEOUT, app.read_response(id)).await??;
    assert_eq!(
        (
            response.summary.already_active_count,
            response.summary.failed_count
        ),
        (1, 0)
    );
    let auth = load_auth_dot_json(
        home.path(),
        AuthCredentialsStoreMode::File,
        AuthKeyringBackendKind::default(),
    )?
    .unwrap();
    let tokens = auth.tokens.unwrap();
    assert_eq!(
        (tokens.access_token.as_str(), tokens.refresh_token.as_str()),
        ("renewed-token", "renewed-refresh-token")
    );
    assert_eq!(tokens.id_token.email.as_deref(), Some("saved@example.com"));
    Ok(())
}

#[test_case(CurrentProvider::OpenAi; "openai_provider")]
#[test_case(CurrentProvider::Other; "unrelated_provider")]
#[tokio::test]
async fn manual_priming_uses_saved_browser_auth_without_tools_or_project_context(
    provider: CurrentProvider,
) -> Result<()> {
    let backend = MockServer::start().await;
    let calls = AtomicUsize::new(0);
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .and(header("authorization", "Bearer saved-token"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200)
                .set_body_json(usage(calls.fetch_add(1, Ordering::SeqCst) > 0))
        })
        .expect(2)
        .mount(&backend)
        .await;
    let inference = responses::mount_sse_once(
        &backend,
        responses::sse(vec![
            responses::ev_response_created("prime"),
            responses::ev_completed("prime"),
        ]),
    )
    .await;
    let (home, mut app, other) = setup(&backend, provider).await?;
    let original = std::fs::read(home.path().join("auth.json"))?;
    let id = app.send_raw_request("accountPriming/runOnce", None).await?;
    let response: AccountPrimingRunOnceResponse =
        timeout(READ_TIMEOUT, app.read_response(id)).await??;
    assert_eq!(
        (
            response.summary.primed_count,
            response.summary.unsupported_count,
            response.summary.failed_count
        ),
        (1, 0, 0)
    );
    assert_eq!(
        response.summary.results[0].profile_name,
        "saved@example.com"
    );
    assert_eq!(
        response.summary.results[0].outcome,
        AccountPrimingProfileOutcome::Primed
    );
    let request = inference.single_request();
    let body = request.body_json();
    assert_eq!(
        json!({
            "instructions": body["instructions"], "input": body["input"],
            "tools": body["tools"], "tool_choice": body["tool_choice"],
        }),
        json!({
            "instructions": "Reply briefly to the greeting.",
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [], "tool_choice": "none",
        })
    );
    assert_eq!(
        request.header("authorization"),
        Some("Bearer saved-token".to_string())
    );
    match provider {
        CurrentProvider::OpenAi => assert_eq!(body["model"], "mock-model"),
        CurrentProvider::Other => assert_ne!(body["model"], "mock-model"),
    }
    assert!(
        other
            .received_requests()
            .await
            .expect("recorded provider requests")
            .iter()
            .all(|request| request.url.path() != "/responses")
    );
    assert_eq!(std::fs::read(home.path().join("auth.json"))?, original);
    assert!(!home.path().join("sessions").exists());
    Ok(())
}

#[tokio::test]
async fn manual_pass_reserves_worker_and_stop_cancels_in_flight_request() -> Result<()> {
    let backend = MockServer::start().await;
    let arrived = Arc::new(Notify::new());
    let request_arrived = Arc::clone(&arrived);
    Mock::given(method("GET"))
        .and(path("/api/codex/usage"))
        .respond_with(move |_: &wiremock::Request| {
            request_arrived.notify_one();
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(30))
                .set_body_json(usage(false))
        })
        .mount(&backend)
        .await;
    let (home, mut app, _other) = setup(&backend, CurrentProvider::OpenAi).await?;
    let original = std::fs::read(home.path().join("auth.json"))?;
    let run = app.send_raw_request("accountPriming/runOnce", None).await?;
    timeout(READ_TIMEOUT, arrived.notified()).await?;
    let read = app.send_raw_request("accountPriming/read", None).await?;
    let status: AccountPrimingReadResponse =
        timeout(READ_TIMEOUT, app.read_response(read)).await??;
    assert_eq!(
        (
            status.status.running,
            status.status.interval_seconds,
            status.status.current_profile_name.as_deref()
        ),
        (true, None, Some("saved@example.com"))
    );
    let duplicate = app
        .send_raw_request(
            "accountPriming/start",
            Some(json!({"intervalSeconds": 300})),
        )
        .await?;
    let error = timeout(
        READ_TIMEOUT,
        app.read_stream_until_error_message(RequestId::Integer(duplicate)),
    )
    .await??;
    assert!(error.error.message.contains("already running"));
    let stop = app.send_raw_request("accountPriming/stop", None).await?;
    let stopped: AccountPrimingStopResponse =
        timeout(READ_TIMEOUT, app.read_response(stop)).await??;
    assert!(!stopped.status.running);
    let summary: AccountPrimingRunOnceResponse =
        timeout(READ_TIMEOUT, app.read_response(run)).await??;
    assert!(summary.summary.cancelled);
    assert_eq!(
        (summary.summary.primed_count, summary.summary.failed_count),
        (0, 0)
    );
    // A completed worker must not clear a newly registered generation, including rapid restarts.
    let start = app
        .send_raw_request(
            "accountPriming/start",
            Some(json!({"intervalSeconds": 300})),
        )
        .await?;
    let restarted: AccountPrimingStartResponse =
        timeout(READ_TIMEOUT, app.read_response(start)).await??;
    assert!(restarted.status.running);
    let stop = app.send_raw_request("accountPriming/stop", None).await?;
    let stopped: AccountPrimingStopResponse =
        timeout(READ_TIMEOUT, app.read_response(stop)).await??;
    assert!(!stopped.status.running);
    assert_eq!(std::fs::read(home.path().join("auth.json"))?, original);
    Ok(())
}
