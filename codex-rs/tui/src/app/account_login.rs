//! Account-add login lifecycle. Login IDs keep canceled or superseded attempts
//! from being reported as failures of the current attempt.

use super::App;
use crate::app_event::AccountLoginMethod;
use crate::app_event::AppEvent;
use crate::app_server_session::AppServerSession;
use codex_app_server_protocol::AccountLoginCompletedNotification;
use codex_app_server_protocol::CancelLoginAccountStatus;
use codex_app_server_protocol::LoginAccountResponse;
use ratatui::style::Stylize;

impl App {
    pub(super) async fn start_account_login(
        &mut self,
        app_server: &mut AppServerSession,
        method: AccountLoginMethod,
    ) {
        if self.account_login_id.is_some() {
            self.cancel_account_login(app_server).await;
            if self.account_login_id.is_some() {
                return;
            }
        }
        match app_server.start_account_session_login(method).await {
            Ok(LoginAccountResponse::Chatgpt { login_id, auth_url }) => {
                self.account_login_id = Some(login_id);
                self.open_url_in_browser(auth_url.clone());
                self.chat_widget.add_plain_history_lines(vec![
                    "Continue signing in with ChatGPT in your browser:".into(),
                    auth_url.cyan().underlined().into(),
                    "Browser unavailable? Use /login device. Use /login cancel to cancel."
                        .dim()
                        .into(),
                ]);
            }
            Ok(LoginAccountResponse::ChatgptDeviceCode {
                login_id,
                verification_url,
                user_code,
            }) => {
                self.account_login_id = Some(login_id);
                self.chat_widget.add_plain_history_lines(vec![
                    "Sign in with ChatGPT using a device code:".bold().into(),
                    "".into(),
                    "1. Open this link on any device and sign in:".into(),
                    verification_url.cyan().underlined().into(),
                    "".into(),
                    "2. Enter this one-time code (expires in 15 minutes):".into(),
                    user_code.cyan().bold().into(),
                    "".into(),
                    "Continue only if you started this login in Codex. If a website or another person gave you this code, cancel."
                        .dim()
                        .into(),
                    "Waiting for sign-in. Use /login cancel to cancel or /login device to request a new code."
                        .dim()
                        .into(),
                ]);
            }
            Ok(other) => self.chat_widget.add_error_message(format!(
                "Unexpected accountSession/login/start response: {other:?}"
            )),
            Err(err) => self
                .chat_widget
                .add_error_message(format!("Login failed: {err:#}. Use /login to try again.")),
        }
    }

    pub(super) async fn cancel_account_login(&mut self, app_server: &mut AppServerSession) {
        let Some(login_id) = self.account_login_id.clone() else {
            self.chat_widget.add_info_message(
                "No account login is in progress.".to_string(),
                /*hint*/ None,
            );
            return;
        };
        match app_server.cancel_account_session_login(login_id).await {
            Ok(status) => {
                self.account_login_id = None;
                let message = match status {
                    CancelLoginAccountStatus::Canceled => "Account login canceled.",
                    CancelLoginAccountStatus::NotFound => {
                        self.app_event_tx.send(AppEvent::ListAccountSessions);
                        "Login is no longer pending. Checking saved accounts."
                    }
                };
                self.chat_widget
                    .add_info_message(message.to_string(), /*hint*/ None);
            }
            Err(err) => self
                .chat_widget
                .add_error_message(format!("Could not cancel login: {err:#}")),
        }
    }

    pub(super) fn on_account_login_completed(
        &mut self,
        notification: &AccountLoginCompletedNotification,
    ) {
        if self.account_login_id.is_none() || notification.login_id != self.account_login_id {
            return;
        }
        self.account_login_id = None;
        if notification.success {
            self.chat_widget.add_info_message(
                "ChatGPT account added. Use /account to view or switch accounts.".to_string(),
                /*hint*/ None,
            );
            self.app_event_tx.send(AppEvent::ListAccountSessions);
        } else {
            self.chat_widget.add_error_message(format!(
                "Login failed: {}. Use /login to try again.",
                notification.error.as_deref().unwrap_or("unknown error")
            ));
        }
    }
}

#[cfg(test)]
#[path = "account_login_tests.rs"]
mod tests;
