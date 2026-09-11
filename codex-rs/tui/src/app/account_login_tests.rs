use super::*;
use crate::app::tests::make_test_app_with_channels;
use crate::app_server_session::ThreadParamsMode;
use codex_app_server_protocol::JSONRPCMessage;
use color_eyre::eyre::Result;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

type ExpectedRpc = (&'static str, Value, Value);

async fn mock_login_server(
    requests: Vec<ExpectedRpc>,
) -> Result<(AppServerSession, JoinHandle<Result<()>>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut requests = requests.into_iter();
        while let Some(frame) = socket.next().await {
            let Message::Text(text) = frame? else {
                continue;
            };
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let mut response = if request.method == "initialize" {
                json!({ "result": { "userAgent": "codex-tui-test" } })
            } else {
                let (method, params, response) = requests.next().expect("expected RPC");
                assert_eq!(
                    (request.method.as_str(), request.params),
                    (method, Some(params))
                );
                response
            };
            response["id"] = serde_json::to_value(request.id)?;
            socket
                .send(Message::Text(response.to_string().into()))
                .await?;
        }
        assert!(requests.next().is_none(), "all expected RPCs were sent");
        Ok(())
    });
    let client = crate::connect_remote_app_server(endpoint).await?;
    Ok((
        AppServerSession::new(client, ThreadParamsMode::Remote),
        server,
    ))
}

fn device_code_request(login_id: &str) -> ExpectedRpc {
    (
        "accountSession/login/start",
        json!({ "type": "chatgptDeviceCode" }),
        json!({ "result": {
            "type": "chatgptDeviceCode", "loginId": login_id,
            "verificationUrl": "https://auth.example.com/codex/device", "userCode": "ABCD-EFGH"
        } }),
    )
}

fn cancel_request(login_id: &str) -> ExpectedRpc {
    (
        "account/login/cancel",
        json!({ "loginId": login_id }),
        json!({ "result": { "status": "canceled" } }),
    )
}

fn login_completed(login_id: &str, result: Result<(), &str>) -> AccountLoginCompletedNotification {
    AccountLoginCompletedNotification {
        login_id: Some(login_id.to_string()),
        success: result.is_ok(),
        error: result.err().map(str::to_string),
        onboarding_entrypoint: None,
    }
}

fn render_history(events: &mut UnboundedReceiver<AppEvent>) -> String {
    let mut lines = Vec::new();
    while let Ok(event) = events.try_recv() {
        let AppEvent::InsertHistoryCell(cell) = event else {
            panic!("unexpected event: {event:?}");
        };
        lines.extend(cell.display_lines(/*width*/ 80));
    }
    let area = Rect::new(
        /*x*/ 0, /*y*/ 0, /*width*/ 80, /*height*/ 24,
    );
    let mut buffer = Buffer::empty(area);
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(area, &mut buffer);
    let rendered = (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|column| buffer[(column, row)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    rendered.trim_end().to_string()
}

#[tokio::test]
async fn device_login_renders_instructions_and_cancels_superseded_attempts() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let (mut session, server) = mock_login_server(vec![
        device_code_request("first"),
        cancel_request("first"),
        device_code_request("second"),
        cancel_request("second"),
    ])
    .await?;

    app.start_account_login(&mut session, AccountLoginMethod::DeviceCode)
        .await;
    assert_eq!(app.account_login_id.as_deref(), Some("first"));
    let instructions = render_history(&mut events);
    assert!(instructions.contains("https://auth.example.com/codex/device"));
    assert!(instructions.contains("ABCD-EFGH"));
    insta::assert_snapshot!("device_code_login_instructions", instructions);

    app.start_account_login(&mut session, AccountLoginMethod::DeviceCode)
        .await;
    assert_eq!(app.account_login_id.as_deref(), Some("second"));
    let _ = render_history(&mut events);
    app.on_account_login_completed(&login_completed("first", Err("canceled")));
    assert_eq!(app.account_login_id.as_deref(), Some("second"));
    assert!(events.try_recv().is_err());

    app.cancel_account_login(&mut session).await;
    assert_eq!(app.account_login_id, None);
    assert!(render_history(&mut events).contains("Account login canceled."));
    app.on_account_login_completed(&login_completed("second", Ok(())));
    assert!(events.try_recv().is_err());
    session.shutdown().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn failed_cancellation_keeps_the_attempt_until_retry_succeeds() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let (mut session, server) = mock_login_server(vec![
        device_code_request("first"),
        (
            "account/login/cancel",
            json!({ "loginId": "first" }),
            json!({ "error": { "code": -32603, "message": "cancellation unavailable" } }),
        ),
        cancel_request("first"),
        device_code_request("second"),
    ])
    .await?;

    app.start_account_login(&mut session, AccountLoginMethod::DeviceCode)
        .await;
    let _ = render_history(&mut events);
    app.start_account_login(&mut session, AccountLoginMethod::DeviceCode)
        .await;
    assert_eq!(app.account_login_id.as_deref(), Some("first"));
    let error = render_history(&mut events);
    assert!(error.contains("Could not cancel login:"));
    assert!(error.contains("cancellation unavailable"));

    app.start_account_login(&mut session, AccountLoginMethod::DeviceCode)
        .await;
    assert_eq!(app.account_login_id.as_deref(), Some("second"));
    session.shutdown().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn login_completion_refreshes_accounts_and_failures_allow_retry() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let (mut session, server) = mock_login_server(vec![
        device_code_request("success"),
        (
            "accountSession/list",
            json!({}),
            json!({ "result": { "activeSessionId": null, "sessions": [] } }),
        ),
        (
            "accountSession/login/start",
            json!({ "type": "chatgptDeviceCode" }),
            json!({ "error": { "code": -32603, "message": "device code unavailable" } }),
        ),
        device_code_request("failure"),
    ])
    .await?;

    app.start_account_login(&mut session, AccountLoginMethod::DeviceCode)
        .await;
    let _ = render_history(&mut events);
    app.on_account_login_completed(&login_completed("success", Ok(())));
    assert_eq!(app.account_login_id, None);
    assert!(matches!(events.try_recv()?, AppEvent::InsertHistoryCell(_)));
    assert!(matches!(events.try_recv()?, AppEvent::ListAccountSessions));
    app.show_account_sessions(&mut session).await;
    assert!(render_history(&mut events).contains("No saved accounts."));

    app.start_account_login(&mut session, AccountLoginMethod::DeviceCode)
        .await;
    assert_eq!(app.account_login_id, None);
    insta::assert_snapshot!("device_code_login_start_error", render_history(&mut events));
    app.start_account_login(&mut session, AccountLoginMethod::DeviceCode)
        .await;
    assert_eq!(app.account_login_id.as_deref(), Some("failure"));
    let _ = render_history(&mut events);
    app.on_account_login_completed(&login_completed("failure", Err("Code expired")));
    assert_eq!(app.account_login_id, None);
    assert_eq!(
        render_history(&mut events),
        "■ Login failed: Code expired. Use /login to try again."
    );
    session.shutdown().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn browser_login_routes_to_account_sessions_and_finished_cancellation_refreshes_accounts()
-> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    let expected = LoginAccountResponse::Chatgpt {
        login_id: "browser".to_string(),
        auth_url: "https://auth.example.com/authorize".to_string(),
    };
    let (mut session, server) = mock_login_server(vec![
        (
            "accountSession/login/start",
            json!({ "type": "chatgpt", "appBrand": null }),
            json!({ "result": serde_json::to_value(&expected)? }),
        ),
        (
            "account/login/cancel",
            json!({ "loginId": "browser" }),
            json!({ "result": { "status": "notFound" } }),
        ),
    ])
    .await?;
    assert_eq!(
        session
            .start_account_session_login(AccountLoginMethod::Browser)
            .await?,
        expected
    );
    app.account_login_id = Some("browser".to_string());
    app.cancel_account_login(&mut session).await;
    assert_eq!(app.account_login_id, None);
    assert!(matches!(events.try_recv()?, AppEvent::ListAccountSessions));
    assert!(render_history(&mut events).contains("Login is no longer pending."));
    session.shutdown().await?;
    server.await??;
    Ok(())
}
