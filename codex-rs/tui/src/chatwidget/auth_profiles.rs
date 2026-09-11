use super::ChatWidget;
use crate::app_event::AppEvent;
use crate::app_event::AuthProfileSwitchTrigger;

impl ChatWidget {
    pub(super) fn request_next_auth_profile(&mut self, trigger: AuthProfileSwitchTrigger) {
        if matches!(trigger, AuthProfileSwitchTrigger::RateLimit) {
            self.set_queue_autosend_suppressed(/*suppressed*/ true);
        }
        self.app_event_tx
            .send(AppEvent::ActivateNextAuthProfile { trigger });
    }

    pub(super) fn show_auth_profile_auto_switch_status(&mut self) {
        let status = if self.auto_switch_auth_profile_on_rate_limit {
            "on"
        } else {
            "off"
        };
        self.add_info_message(
            format!("Automatic account switching on rate limits is {status} for this session."),
            /*hint*/ None,
        );
    }

    pub(super) fn set_auto_switch_auth_profile_on_rate_limit(&mut self, enabled: bool) {
        self.auto_switch_auth_profile_on_rate_limit = enabled;
    }

    pub(crate) fn on_auth_profile_next_activated(
        &mut self,
        trigger: AuthProfileSwitchTrigger,
        result: Result<codex_app_server_protocol::AuthProfileActivateNextResponse, String>,
    ) {
        if matches!(trigger, AuthProfileSwitchTrigger::RateLimit) {
            self.set_queue_autosend_suppressed(/*suppressed*/ false);
        }

        match result {
            Ok(response) => {
                self.clear_token_usage();
                self.rate_limit_snapshots_by_limit_id.clear();
                let message = match trigger {
                    AuthProfileSwitchTrigger::ManualNext => {
                        format!("Activated next account {}", response.profile.name)
                    }
                    AuthProfileSwitchTrigger::RateLimit => {
                        format!(
                            "Rate limit hit. Switched to account {}",
                            response.profile.name
                        )
                    }
                };
                self.add_info_message(message, /*hint*/ None);
                if matches!(trigger, AuthProfileSwitchTrigger::RateLimit) {
                    self.maybe_send_next_queued_input();
                }
            }
            Err(err) => {
                if matches!(trigger, AuthProfileSwitchTrigger::RateLimit) {
                    self.disable_stop_loop_due_to_limit();
                }
                let prefix = match trigger {
                    AuthProfileSwitchTrigger::ManualNext => "Failed to activate next account",
                    AuthProfileSwitchTrigger::RateLimit => {
                        "Failed to automatically switch account after rate limit"
                    }
                };
                self.add_error_message(format!("{prefix}: {err}"));
            }
        }
    }
}
