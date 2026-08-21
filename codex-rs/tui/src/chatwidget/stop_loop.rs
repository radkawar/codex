use super::*;

impl ChatWidget {
    pub(super) fn set_stop_loop(&mut self, mode: StopLoopMode, prompt: String) {
        let trimmed = prompt.trim().to_string();
        if trimmed.is_empty() {
            self.add_error_message("Loop prompt cannot be empty.".to_string());
            return;
        }
        self.stop_loop = Some(StopLoopConfig {
            prompt: trimmed.clone(),
            mode,
        });
        let scope = match mode {
            StopLoopMode::Once => "next successful turn",
            StopLoopMode::Always => "every successful turn",
        };
        self.add_info_message(
            format!("Loop armed for {scope}: {trimmed}"),
            /*hint*/ None,
        );
    }

    pub(super) fn show_stop_loop_status(&mut self) {
        let message = match &self.stop_loop {
            Some(config) => match config.mode {
                StopLoopMode::Once => format!("Loop is armed once: {}", config.prompt),
                StopLoopMode::Always => format!("Loop is armed persistently: {}", config.prompt),
            },
            None => "Loop is off.".to_string(),
        };
        self.add_info_message(message, /*hint*/ None);
    }

    pub(super) fn maybe_run_stop_loop(&mut self) -> bool {
        if self.bottom_pane.is_task_running() || self.has_queued_follow_up_messages() {
            return false;
        }

        let Some(config) = self.stop_loop.clone() else {
            return false;
        };

        if matches!(config.mode, StopLoopMode::Once) {
            self.stop_loop = None;
        }
        self.submit_user_message(config.prompt.into());
        true
    }

    pub(super) fn disable_stop_loop_due_to_limit(&mut self) {
        if self.stop_loop.take().is_some() {
            self.add_to_history(history_cell::new_warning_event(
                "Loop disabled after a rate limit or quota error.".to_string(),
            ));
            self.request_redraw();
        }
    }
}
