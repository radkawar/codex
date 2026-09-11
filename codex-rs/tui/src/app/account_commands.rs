use super::App;
use crate::app_server_session::AppServerSession;
use crate::status::format_reset_timestamp;
use crate::status::plan_type_display_name;
use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use codex_app_server_protocol::Account;
use codex_app_server_protocol::AccountSessionsResponse;
use codex_app_server_protocol::RateLimitWindow;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

impl App {
    pub(super) async fn show_account_sessions(&mut self, app_server: &mut AppServerSession) {
        match app_server.list_account_sessions().await {
            Ok(response) => self.show_account_sessions_response(response),
            Err(err) => self
                .chat_widget
                .add_error_message(format!("Could not list accounts: {err}")),
        }
    }

    pub(super) async fn switch_account_session(
        &mut self,
        app_server: &mut AppServerSession,
        session_id: String,
        account_id: Option<String>,
    ) {
        match app_server
            .switch_account_session(session_id, account_id)
            .await
        {
            Ok(response) => {
                self.chat_widget.add_info_message(
                    "Account switched. New turns will use the selected account.".to_string(),
                    /*hint*/ None,
                );
                self.show_account_sessions_response(response);
            }
            Err(err) => self
                .chat_widget
                .add_error_message(format!("Could not switch accounts: {err}")),
        }
    }

    pub(super) async fn logout_account_session(
        &mut self,
        app_server: &mut AppServerSession,
        session_id: String,
    ) {
        match app_server.logout_account_session(session_id).await {
            Ok(response) => {
                self.chat_widget
                    .add_info_message("Saved account logged out.".to_string(), /*hint*/ None);
                self.show_account_sessions_response(response);
            }
            Err(err) => self
                .chat_widget
                .add_error_message(format!("Could not log out account: {err}")),
        }
    }

    fn show_account_sessions_response(&mut self, response: AccountSessionsResponse) {
        self.chat_widget.add_info_message(
            format_account_sessions(&response, Local::now()),
            Some(
                "Use /login to add an account, /account switch <email|id> [workspace-id] to switch, or /account logout <email|id>. Account-changing commands entered during a turn run after that turn, before the next command."
                    .to_string(),
            ),
        );
    }
}

fn format_account_sessions(
    response: &AccountSessionsResponse,
    captured_at: DateTime<Local>,
) -> String {
    if response.sessions.is_empty() {
        return "No saved accounts.".to_string();
    }

    let rows = response
        .sessions
        .iter()
        .map(|session| {
            let (account, plan) = match &session.account {
                Some(Account::ApiKey {}) => ("API key".to_string(), "-".to_string()),
                Some(Account::Chatgpt { email, plan_type }) => (
                    email
                        .as_ref()
                        .or(session.email.as_ref())
                        .cloned()
                        .unwrap_or_else(|| session.session_id.clone()),
                    plan_type_display_name(*plan_type),
                ),
                Some(Account::AmazonBedrock { .. }) => {
                    ("Amazon Bedrock".to_string(), "-".to_string())
                }
                None => (
                    session
                        .email
                        .as_ref()
                        .or(session.display_name.as_ref())
                        .unwrap_or(&session.session_id)
                        .clone(),
                    "-".to_string(),
                ),
            };
            let five_hour = session
                .rate_limits
                .as_ref()
                .and_then(|limits| limits.primary.as_ref());
            let weekly = session
                .rate_limits
                .as_ref()
                .and_then(|limits| limits.secondary.as_ref());
            [
                if session.is_active {
                    "*".to_string()
                } else {
                    String::new()
                },
                account,
                plan,
                format_window_left(five_hour),
                format_window_reset(five_hour, captured_at),
                format_window_left(weekly),
                format_window_reset(weekly, captured_at),
            ]
        })
        .collect::<Vec<_>>();
    let headers = [
        "CUR",
        "ACCOUNT",
        "PLAN",
        "5H LEFT",
        "5H RESET",
        "WEEKLY LEFT",
        "WEEKLY RESET",
    ];
    let max_widths = [3, 32, 10, 7, 16, 11, 16];
    let widths: [usize; 7] = std::array::from_fn(|index| {
        column_width(
            headers[index],
            rows.iter().map(|row| row[index].as_str()),
            max_widths[index],
        )
    });
    let mut lines = vec![
        "Saved accounts (* = current):".to_string(),
        join_table_cells(
            headers
                .iter()
                .zip(widths)
                .map(|(header, width)| pad_table_cell(header, width)),
        ),
        join_table_cells(widths.map(|width| "-".repeat(width))),
    ];
    for (session, row) in response.sessions.iter().zip(rows) {
        lines.push(join_table_cells(
            row.iter()
                .zip(widths)
                .map(|(cell, width)| pad_table_cell(cell, width)),
        ));
        lines.push(format!("    ID: {}", session.session_id));
        for workspace in &session.workspaces {
            let selected = session.selected_workspace_account_id.as_deref()
                == Some(workspace.account_id.as_str());
            let marker = if selected { "→" } else { " " };
            let name = workspace.name.as_deref().unwrap_or("unnamed workspace");
            lines.push(format!("    {marker} {name} ({})", workspace.account_id));
        }
    }
    lines.join("\n")
}

fn format_window_left(window: Option<&RateLimitWindow>) -> String {
    match window {
        Some(window) => format!("{}%", (100 - window.used_percent).clamp(0, 100)),
        None => "-".to_string(),
    }
}

fn format_window_reset(window: Option<&RateLimitWindow>, captured_at: DateTime<Local>) -> String {
    window
        .and_then(|window| window.resets_at)
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map(|dt| format_reset_timestamp(dt.with_timezone(&Local), captured_at))
        .unwrap_or_else(|| "-".to_string())
}

fn column_width<'a>(
    header: &str,
    values: impl Iterator<Item = &'a str>,
    max_width: usize,
) -> usize {
    let content_width = values
        .map(UnicodeWidthStr::width)
        .max()
        .unwrap_or(0)
        .max(UnicodeWidthStr::width(header));
    content_width.min(max_width)
}

fn join_table_cells(cells: impl IntoIterator<Item = String>) -> String {
    cells
        .into_iter()
        .collect::<Vec<_>>()
        .join("  ")
        .trim_end()
        .to_string()
}

fn pad_table_cell(text: &str, width: usize) -> String {
    let truncated = truncate_display_width(text, width);
    let padding = width.saturating_sub(UnicodeWidthStr::width(truncated.as_str()));
    format!("{truncated}{}", " ".repeat(padding))
}

fn truncate_display_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let mut out = String::new();
    let mut used = 0usize;
    for grapheme in text.graphemes(true) {
        let width = UnicodeWidthStr::width(grapheme);
        if used + width > max_width.saturating_sub(1) {
            break;
        }
        out.push_str(grapheme);
        used += width;
    }
    out.push('…');
    out
}

#[cfg(test)]
#[path = "account_commands_tests.rs"]
mod tests;
