use super::App;
use crate::app_server_session::AppServerSession;
use codex_app_server_protocol::AccountSession;
use codex_app_server_protocol::AccountSessionsResponse;
use codex_app_server_protocol::LoginAccountResponse;

impl App {
    pub(super) async fn start_account_login(&mut self, app_server: &mut AppServerSession) {
        match app_server.start_account_session_login().await {
            Ok(LoginAccountResponse::Chatgpt { auth_url, .. }) => {
                self.open_url_in_browser(auth_url.clone());
                self.chat_widget.add_info_message(
                    "Continue signing in with ChatGPT in your browser.".to_string(),
                    Some(auth_url),
                );
            }
            Ok(other) => self.chat_widget.add_error_message(format!(
                "Unexpected accountSession/login/start response: {other:?}"
            )),
            Err(err) => self
                .chat_widget
                .add_error_message(format!("Login failed: {err}")),
        }
    }

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
            format_account_sessions(&response),
            Some(
                "Use /login to add an account, /account session switch <session-id> [workspace-id] to switch, or /account session logout <session-id>. Account-changing commands entered during a turn run after that turn, before the next command."
                    .to_string(),
            ),
        );
    }
}

fn format_account_sessions(response: &AccountSessionsResponse) -> String {
    if response.sessions.is_empty() {
        return "No saved ChatGPT accounts.".to_string();
    }

    let mut lines = vec!["Saved ChatGPT accounts:".to_string()];
    for session in &response.sessions {
        lines.push(format_account_session(session));
        for workspace in &session.workspaces {
            let selected = session.selected_workspace_account_id.as_deref()
                == Some(workspace.account_id.as_str());
            let marker = if selected { "  →" } else { "   " };
            let name = workspace.name.as_deref().unwrap_or("unnamed workspace");
            lines.push(format!("{marker} {name} ({})", workspace.account_id));
        }
    }
    lines.join("\n")
}

fn format_account_session(session: &AccountSession) -> String {
    let marker = if session.is_active { "*" } else { " " };
    let identity = session
        .display_name
        .as_deref()
        .or(session.email.as_deref())
        .or(session.user_id.as_deref())
        .unwrap_or("unknown account");
    let email = session
        .email
        .as_deref()
        .filter(|email| *email != identity)
        .map(|email| format!(" <{email}>"))
        .unwrap_or_default();
    format!("{marker} {identity}{email} [{}]", session.session_id)
}

#[cfg(test)]
#[path = "account_commands_tests.rs"]
mod tests;
