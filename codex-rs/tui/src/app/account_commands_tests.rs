use super::*;
use chrono::Duration;
use chrono::TimeZone;
use codex_app_server_protocol::AccountSession;
use codex_app_server_protocol::AccountSessionWorkspace;
use codex_app_server_protocol::RateLimitSnapshot;
use codex_protocol::account::PlanType;

#[test]
fn formats_active_account_and_selected_workspace_without_credentials() {
    let captured_at = Local
        .with_ymd_and_hms(2026, 9, 11, 12, 0, 0)
        .single()
        .unwrap();
    let response = AccountSessionsResponse {
        active_session_id: Some("account-1".to_string()),
        sessions: vec![
            AccountSession {
                session_id: "account-1".to_string(),
                account: Some(Account::Chatgpt {
                    email: Some("user@example.com".to_string()),
                    plan_type: PlanType::Pro,
                }),
                rate_limits: Some(RateLimitSnapshot {
                    limit_id: None,
                    limit_name: None,
                    normal_model_slug: None,
                    primary: Some(RateLimitWindow {
                        used_percent: 25,
                        window_duration_mins: Some(300),
                        resets_at: Some((captured_at + Duration::hours(5)).timestamp()),
                    }),
                    secondary: Some(RateLimitWindow {
                        used_percent: 64,
                        window_duration_mins: Some(10080),
                        resets_at: Some((captured_at + Duration::days(7)).timestamp()),
                    }),
                    credits: None,
                    individual_limit: None,
                    spend_control_reached: None,
                    plan_type: Some(PlanType::Pro),
                    rate_limit_reached_type: None,
                }),
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
            },
            AccountSession {
                session_id: "account-2".to_string(),
                account: Some(Account::ApiKey {}),
                rate_limits: None,
                email: None,
                user_id: None,
                display_name: None,
                image_url: None,
                last_used_at: 0,
                is_active: false,
                selected_workspace_account_id: None,
                workspaces: Vec::new(),
            },
        ],
    };

    insta::assert_snapshot!(format_account_sessions(&response, captured_at));
}
