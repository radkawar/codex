//! Isolates credential refreshes until the canonical saved session accepts them.

use crate::account_sessions::AccountSessionCredentialsSnapshot;
use crate::account_sessions::AccountSessionsStore;
use codex_backend_client::Client as BackendClient;
use codex_backend_client::RequestError;
use codex_core::config::Config;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthManager;
use codex_login::AuthManagerConfig;
use codex_login::CodexAuth;
use codex_login::load_auth_dot_json;
use codex_login::save_auth;
use codex_protocol::auth::AuthMode;
use codex_protocol::protocol::RateLimitSnapshot;
use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

type SessionLocks = HashMap<(PathBuf, String), Weak<tokio::sync::Mutex<()>>>;
static SESSION_LOCKS: LazyLock<Mutex<SessionLocks>> = LazyLock::new(Mutex::default);

#[cfg(test)]
#[path = "account_session_auth_tests.rs"]
mod tests;

pub(crate) struct IsolatedAccountSessionAuth {
    pub(crate) auth_manager: Arc<AuthManager>,
    snapshot: AccountSessionCredentialsSnapshot,
    credentials: IsolatedCredentials,
    _session_lock: OwnedMutexGuard<()>,
}

struct IsolatedCredentials {
    home: TempDir,
    store_mode: AuthCredentialsStoreMode,
    keyring_backend: codex_login::AuthKeyringBackendKind,
}

impl IsolatedAccountSessionAuth {
    pub(crate) async fn new(
        config: &Config,
        store: &AccountSessionsStore<'_>,
        session_id: &str,
    ) -> io::Result<Self> {
        let lock = {
            let mut locks = SESSION_LOCKS
                .lock()
                .map_err(|_| io::Error::other("account refresh lock poisoned"))?;
            locks.retain(|_, lock| lock.strong_count() > 0);
            let key = (config.codex_home.to_path_buf(), session_id.to_string());
            let lock = locks.get(&key).and_then(Weak::upgrade).unwrap_or_default();
            locks.insert(key, Arc::downgrade(&lock));
            lock
        };
        let session_lock = lock.lock_owned().await;
        // Snapshot only after acquiring the lock: an earlier reader may have rotated tokens.
        let snapshot = store.snapshot_session_credentials(session_id).await?;
        let mut builder = tempfile::Builder::new();
        builder.prefix("codex-account-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let credentials = builder.tempdir()?;
        let keyring_backend = config.auth_keyring_backend_kind();
        let store_mode = if snapshot.auth.auth_mode == Some(AuthMode::ChatgptAuthTokens) {
            AuthCredentialsStoreMode::Ephemeral
        } else {
            AuthCredentialsStoreMode::File
        };
        save_auth(
            credentials.path(),
            &snapshot.auth,
            store_mode,
            keyring_backend,
        )?;
        // This guard also cleans ephemeral auth if construction is canceled at the await below.
        let credentials = IsolatedCredentials {
            home: credentials,
            store_mode,
            keyring_backend,
        };
        let auth_manager = AuthManager::shared(
            credentials.home.path().to_path_buf(),
            /*enable_codex_api_key_env*/ false,
            store_mode,
            config
                .managed_auth_policy()
                .effective_chatgpt_workspaces(config.forced_chatgpt_workspace_id.as_deref()),
            Some(config.chatgpt_base_url.clone()),
            keyring_backend,
            config.auth_route_config(),
        )
        .await;
        Ok(Self {
            auth_manager,
            snapshot,
            credentials,
            _session_lock: session_lock,
        })
    }

    pub(crate) fn initial_auth(&self) -> &AuthDotJson {
        &self.snapshot.auth
    }

    pub(crate) async fn fetch_rate_limits(
        &self,
        config: &Config,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> io::Result<RateLimitSnapshot> {
        // A refresh can rotate tokens on the server. Finish it before honoring cancellation.
        let auth = self
            .auth_manager
            .auth()
            .await
            .ok_or_else(|| io::Error::other("saved account has no auth"))?;
        if !auth.uses_codex_backend() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "saved account has no ChatGPT usage windows",
            ));
        }
        let managed = matches!(auth, CodexAuth::Chatgpt(_));
        let fetch = |auth: CodexAuth| async move {
            let client = BackendClient::from_auth(
                &config.chatgpt_base_url,
                &auth,
                config.http_client_factory(),
            );
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "account usage request canceled")),
                result = tokio::time::timeout(timeout, client.get_rate_limits_many()) => {
                    result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "account usage request timed out"))
                }
            }
        };
        let snapshots = match fetch(auth).await? {
            Ok(snapshots) => snapshots,
            Err(error)
                if managed
                    && error
                        .downcast_ref::<RequestError>()
                        .is_some_and(RequestError::is_unauthorized)
                    && !cancel.is_cancelled() =>
            {
                self.auth_manager
                    .refresh_token()
                    .await
                    .map_err(io::Error::other)?;
                let auth = self
                    .auth_manager
                    .auth_cached()
                    .ok_or_else(|| io::Error::other("saved account has no auth"))?;
                fetch(auth).await?.map_err(io::Error::other)?
            }
            Err(error) => return Err(io::Error::other(error)),
        };
        snapshots
            .iter()
            .find(|snapshot| snapshot.limit_id.as_deref() == Some("codex"))
            .or_else(|| snapshots.first())
            .cloned()
            .ok_or_else(|| io::Error::other("no rate-limit snapshots returned"))
    }

    pub(crate) async fn persist_if_changed(
        &self,
        store: &AccountSessionsStore<'_>,
    ) -> io::Result<bool> {
        let refreshed = load_auth_dot_json(
            self.credentials.home.path(),
            self.credentials.store_mode,
            self.credentials.keyring_backend,
        )?
        .ok_or_else(|| io::Error::other("isolated account credentials disappeared"))?;
        if refreshed == self.snapshot.auth {
            return Ok(false);
        }
        store
            .persist_session_credentials_if_unchanged(&self.snapshot, &refreshed)
            .await
    }
}

impl Drop for IsolatedCredentials {
    fn drop(&mut self) {
        if self.store_mode == AuthCredentialsStoreMode::Ephemeral
            && let Err(error) =
                codex_login::logout(self.home.path(), self.store_mode, self.keyring_backend)
        {
            tracing::warn!("failed to discard isolated account auth: {error}");
        }
    }
}
