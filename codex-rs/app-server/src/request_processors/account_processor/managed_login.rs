//! Keeps managed OAuth credentials private until completion wins against cancellation.

use super::ActiveLogin;
use super::ChatgptLoginDestination;
use crate::account_sessions::AccountSessionActivation;
use crate::account_sessions::AccountSessionsStore;
use codex_app_server_protocol::AccountLoginCompletedNotification;
use codex_core::config::Config;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthManager;
use codex_login::ServerOptions;
use codex_login::load_auth_dot_json;
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;
use uuid::Uuid;

pub(super) struct ManagedLoginAttempt {
    pub(super) login_id: Uuid,
    credentials: TempDir,
    active_login: Arc<Mutex<Option<ActiveLogin>>>,
}

impl ManagedLoginAttempt {
    pub(super) fn stage(
        options: &mut ServerOptions,
        active_login: Arc<Mutex<Option<ActiveLogin>>>,
    ) -> std::io::Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("codex-login-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(/*mode*/ 0o700));
        }
        let credentials = builder.tempdir()?;
        options.codex_home = credentials.path().to_path_buf();
        options.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
        Ok(Self {
            login_id: Uuid::new_v4(),
            credentials,
            active_login,
        })
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "completion must commit account storage before cancellation can report its result"
    )]
    pub(super) async fn finish(
        &self,
        config: &Config,
        auth_manager: &Arc<AuthManager>,
        destination: ChatgptLoginDestination,
        notification: &mut AccountLoginCompletedNotification,
    ) {
        // Cancellation must either prevent import or observe its completed state.
        // Keep the lock until both storage and the shared auth cache are committed.
        let mut active = self.active_login.lock().await;
        if active.as_ref().map(ActiveLogin::login_id) != Some(self.login_id) {
            notification.success = false;
            notification.error = Some("Login was not completed".to_string());
            return;
        }
        if notification.success {
            let result = async {
                let auth = load_auth_dot_json(
                    self.credentials.path(),
                    AuthCredentialsStoreMode::File,
                    config.auth_keyring_backend_kind(),
                )?
                .ok_or_else(|| std::io::Error::other("Login credentials were not saved"))?;
                let activation = match destination {
                    ChatgptLoginDestination::Activate => AccountSessionActivation::Activate,
                    ChatgptLoginDestination::AddWithoutSwitching => {
                        AccountSessionActivation::PreserveCurrent
                    }
                };
                AccountSessionsStore::new(config, auth_manager)
                    .add_auth(&auth, activation)
                    .await?;
                auth_manager.reload().await;
                Ok::<_, std::io::Error>(())
            }
            .await;
            if let Err(error) = result {
                notification.success = false;
                notification.error = Some(format!("failed to save account session: {error}"));
            }
        }
        *active = None;
    }
}
