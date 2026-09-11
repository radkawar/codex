//! Sign-in method selection for the in-session `/login` command.

use super::ChatWidget;
use crate::app_event::AccountLoginMethod;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;

impl ChatWidget {
    pub(super) fn handle_account_login_command(&mut self, args: &str) {
        match args.trim() {
            "" => {
                self.bottom_pane.show_selection_view(SelectionViewParams {
                    title: Some("Add a ChatGPT account".to_string()),
                    subtitle: Some("Choose how to sign in.".to_string()),
                    items: vec![
                        SelectionItem {
                            name: "Browser".to_string(),
                            description: Some("Open a browser on this computer".to_string()),
                            actions: vec![Box::new(|tx| {
                                tx.send(AppEvent::StartAccountLogin {
                                    method: AccountLoginMethod::Browser,
                                });
                            })],
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                        SelectionItem {
                            name: "Device code".to_string(),
                            description: Some(
                                "Use a code in a browser on any device (/login device)".to_string(),
                            ),
                            actions: vec![Box::new(|tx| {
                                tx.send(AppEvent::StartAccountLogin {
                                    method: AccountLoginMethod::DeviceCode,
                                });
                            })],
                            dismiss_on_select: true,
                            ..Default::default()
                        },
                    ],
                    ..Default::default()
                });
                self.defer_input_until_settings_applied();
                self.request_redraw();
            }
            "browser" => self.app_event_tx.send(AppEvent::StartAccountLogin {
                method: AccountLoginMethod::Browser,
            }),
            "device" | "--device-auth" => self.app_event_tx.send(AppEvent::StartAccountLogin {
                method: AccountLoginMethod::DeviceCode,
            }),
            "cancel" => self.app_event_tx.send(AppEvent::CancelAccountLogin),
            _ => self.add_error_message("Usage: /login [browser|device|cancel]".to_string()),
        }
    }
}
