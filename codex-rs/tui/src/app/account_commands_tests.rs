use super::*;
use codex_app_server_protocol::AccountSessionWorkspace;
use pretty_assertions::assert_eq;

#[test]
fn formats_active_account_and_selected_workspace_without_credentials() {
    let response = AccountSessionsResponse {
        active_session_id: Some("session-1".to_string()),
        sessions: vec![AccountSession {
            session_id: "session-1".to_string(),
            email: Some("user@example.com".to_string()),
            user_id: Some("user-1".to_string()),
            display_name: Some("Codex User".to_string()),
            image_url: None,
            last_used_at: 0,
            is_active: true,
            selected_workspace_account_id: Some("workspace-1".to_string()),
            workspaces: vec![AccountSessionWorkspace {
                account_id: "workspace-1".to_string(),
                name: Some("Example Team".to_string()),
                image_url: None,
                kind: None,
            }],
        }],
    };

    assert_eq!(
        format_account_sessions(&response),
        "Saved ChatGPT accounts:\n* Codex User <user@example.com> [session-1]\n  → Example Team (workspace-1)"
    );
    insta::assert_snapshot!(format_account_sessions(&response));
}
