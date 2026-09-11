use super::*;
use crate::auth_profiles::account_from_auth;
use crate::auth_profiles::auth_mode_from_auth;
use codex_app_server_protocol::Account;
use codex_backend_client::AccountEntry;
use codex_backend_client::Client as BackendClient;
use codex_login::AuthCredentialsStoreMode;
use codex_protocol::auth::AuthMode;
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use uuid::Uuid;

impl AccountSessionsStore<'_> {
    pub(super) async fn refresh_workspace_metadata(&self, session: &mut StoredAccountSession) {
        let manager = self
            .session_auth_manager(self.session_home(&session.session_id))
            .await;
        let Some(auth) = manager.auth().await else {
            return;
        };
        if !matches!(auth, CodexAuth::Chatgpt(_)) {
            return;
        }
        let Ok(token_data) = auth.get_token_data() else {
            return;
        };
        let mut client = BackendClient::from_auth(
            self.config.chatgpt_base_url.clone(),
            &auth,
            self.config.http_client_factory(),
        );
        if let Some(account_id) = token_data.account_id.as_ref() {
            client = client.with_chatgpt_account_id(account_id);
        }
        let Ok(accounts) = client.get_accounts_check().await else {
            return;
        };
        session.selected_workspace_account_id = token_data
            .account_id
            .or(accounts.default_account_id)
            .or_else(|| accounts.account_ordering.first().cloned());
        session.workspaces = accounts
            .accounts
            .into_iter()
            .map(Self::workspace_from_account)
            .collect();
    }

    pub(super) async fn session_auth_manager(&self, session_home: PathBuf) -> Arc<AuthManager> {
        AuthManager::shared(
            session_home,
            /*enable_codex_api_key_env*/ false,
            self.config.cli_auth_credentials_store_mode,
            self.auth_manager.effective_chatgpt_workspaces(),
            Some(self.config.chatgpt_base_url.clone()),
            self.config.auth_keyring_backend_kind(),
            self.config.auth_route_config(),
        )
        .await
    }

    pub(super) async fn revoke_and_delete_session_auth(
        &self,
        session_id: &str,
    ) -> std::io::Result<()> {
        let session_home = self.session_home(session_id);
        let manager = self.session_auth_manager(session_home.clone()).await;
        manager.logout_with_revoke().await?;
        match std::fs::remove_dir(&session_home) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err),
        }
    }

    pub(super) fn session_from_auth_json(auth_json: &AuthDotJson) -> StoredAccountSession {
        let tokens = auth_json.tokens.as_ref();
        let selected_workspace_account_id = tokens.and_then(|tokens| {
            tokens
                .account_id
                .clone()
                .or_else(|| tokens.id_token.chatgpt_account_id.clone())
        });
        let workspaces = selected_workspace_account_id
            .as_ref()
            .map(|account_id| {
                vec![AccountSessionWorkspace {
                    account_id: account_id.clone(),
                    name: None,
                    image_url: None,
                    kind: None,
                }]
            })
            .unwrap_or_default();
        StoredAccountSession {
            session_id: Uuid::now_v7().to_string(),
            email: match account_from_auth(auth_json) {
                Some(Account::Chatgpt { email, .. }) => email,
                Some(Account::ApiKey {} | Account::AmazonBedrock { .. }) | None => None,
            },
            user_id: tokens.and_then(|tokens| tokens.id_token.chatgpt_user_id.clone()),
            display_name: None,
            image_url: None,
            last_used_at: Utc::now().timestamp(),
            selected_workspace_account_id,
            workspaces,
        }
    }

    pub(super) fn find_matching_session(
        &self,
        stored: &StoredAccountSessions,
        auth_json: &AuthDotJson,
    ) -> std::io::Result<Option<usize>> {
        let mut comparable_auth = auth_json.clone();
        let auth_mode = auth_mode_from_auth(auth_json);
        comparable_auth.auth_mode = None;
        comparable_auth.last_refresh = None;
        for (index, session) in stored.sessions.iter().enumerate() {
            let Some(mut saved_auth) = self.load_session_auth(&session.session_id)? else {
                continue;
            };
            let managed = Self::is_managed_chatgpt_auth_json(auth_json);
            let saved_managed = Self::is_managed_chatgpt_auth_json(&saved_auth);
            let saved_auth_mode = auth_mode_from_auth(&saved_auth);
            saved_auth.auth_mode = None;
            saved_auth.last_refresh = None;
            if (managed && saved_managed && Self::same_identity(session, auth_json))
                || (saved_auth_mode == auth_mode && saved_auth == comparable_auth)
            {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }

    pub(super) fn same_identity(session: &StoredAccountSession, auth_json: &AuthDotJson) -> bool {
        let Some(tokens) = auth_json.tokens.as_ref() else {
            return false;
        };
        match (
            session.user_id.as_deref(),
            tokens.id_token.chatgpt_user_id.as_deref(),
        ) {
            (Some(saved), Some(active)) => saved == active,
            _ => session
                .email
                .as_ref()
                .zip(tokens.id_token.email.as_ref())
                .is_some_and(|(saved, active)| saved.eq_ignore_ascii_case(active)),
        }
    }

    pub(super) fn is_managed_chatgpt_auth_json(auth_json: &AuthDotJson) -> bool {
        // Browser OAuth can store an exchanged API key alongside managed ChatGPT
        // tokens. An explicit auth mode takes precedence over that companion key.
        (auth_json.auth_mode == Some(AuthMode::Chatgpt)
            || auth_json.auth_mode.is_none() && auth_json.openai_api_key.is_none())
            && auth_json.tokens.is_some()
            && auth_json.agent_identity.is_none()
            && auth_json.personal_access_token.is_none()
            && auth_json.bedrock_api_key.is_none()
            && auth_json.bedrock_access_keys.is_none()
    }

    pub(super) fn workspace_from_account(account: AccountEntry) -> AccountSessionWorkspace {
        let kind = match account.structure.as_str() {
            "personal" => Some(AccountSessionWorkspaceKind::Personal),
            "workspace" => Some(AccountSessionWorkspaceKind::Workspace),
            _ => None,
        };
        AccountSessionWorkspace {
            account_id: account.id,
            name: account.name,
            image_url: account.profile_picture_url,
            kind,
        }
    }

    pub(super) fn response(
        &self,
        stored: StoredAccountSessions,
    ) -> std::io::Result<AccountSessionsResponse> {
        let active_session_id = stored.active_session_id;
        let mut sessions = stored
            .sessions
            .into_iter()
            .map(|session| {
                let auth = self.load_session_auth(&session.session_id)?;
                Ok(AccountSession {
                    account: auth.as_ref().and_then(account_from_auth),
                    rate_limits: None,
                    is_active: Some(&session.session_id) == active_session_id.as_ref(),
                    session_id: session.session_id,
                    email: session.email,
                    user_id: session.user_id,
                    display_name: session.display_name,
                    image_url: session.image_url,
                    last_used_at: session.last_used_at,
                    selected_workspace_account_id: session.selected_workspace_account_id,
                    workspaces: session.workspaces,
                })
            })
            .collect::<std::io::Result<Vec<_>>>()?;
        sessions.sort_by_key(|session| std::cmp::Reverse(session.last_used_at));
        Ok(AccountSessionsResponse {
            active_session_id,
            sessions,
        })
    }

    pub(super) fn load_active_auth(&self) -> std::io::Result<Option<AuthDotJson>> {
        if let Some(auth) = load_auth_dot_json(
            &self.config.codex_home,
            AuthCredentialsStoreMode::Ephemeral,
            self.config.auth_keyring_backend_kind(),
        )? {
            return Ok(Some(auth));
        }
        load_auth_dot_json(
            &self.config.codex_home,
            self.config.cli_auth_credentials_store_mode,
            self.config.auth_keyring_backend_kind(),
        )
    }

    pub(super) fn load_session_auth(
        &self,
        session_id: &str,
    ) -> std::io::Result<Option<AuthDotJson>> {
        load_auth_dot_json(
            &self.session_home(session_id),
            self.config.cli_auth_credentials_store_mode,
            self.config.auth_keyring_backend_kind(),
        )
    }

    pub(super) fn save_session_auth(
        &self,
        session_id: &str,
        auth_json: &AuthDotJson,
    ) -> std::io::Result<()> {
        let session_home = self.session_home(session_id);
        std::fs::create_dir_all(&session_home)?;
        save_auth(
            &session_home,
            auth_json,
            self.config.cli_auth_credentials_store_mode,
            self.config.auth_keyring_backend_kind(),
        )
    }

    pub(super) fn save_session_as_active(&self, session_id: &str) -> std::io::Result<()> {
        let auth_json = self
            .load_session_auth(session_id)?
            .ok_or_else(|| std::io::Error::other("Saved account session has no auth"))?;
        let target_store_mode = if auth_json.auth_mode == Some(AuthMode::ChatgptAuthTokens) {
            AuthCredentialsStoreMode::Ephemeral
        } else {
            self.config.cli_auth_credentials_store_mode
        };
        save_auth(
            &self.config.codex_home,
            &auth_json,
            target_store_mode,
            self.config.auth_keyring_backend_kind(),
        )?;
        if target_store_mode != AuthCredentialsStoreMode::Ephemeral {
            codex_login::logout(
                &self.config.codex_home,
                AuthCredentialsStoreMode::Ephemeral,
                self.config.auth_keyring_backend_kind(),
            )?;
        }
        if target_store_mode != self.config.cli_auth_credentials_store_mode {
            codex_login::logout(
                &self.config.codex_home,
                self.config.cli_auth_credentials_store_mode,
                self.config.auth_keyring_backend_kind(),
            )?;
        }
        Ok(())
    }

    pub(super) async fn acquire_lock(&self) -> std::io::Result<AccountSessionsLock> {
        std::fs::create_dir_all(&self.config.codex_home)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(self.config.codex_home.join(ACCOUNT_SESSIONS_LOCK_FILE))?;
        let locked = tokio::time::timeout(
            ACCOUNT_SESSIONS_LOCK_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                File::lock(&file)?;
                Ok::<File, std::io::Error>(file)
            }),
        )
        .await
        .map_err(|_| std::io::Error::other("timed out waiting for account session storage lock"))?
        .map_err(std::io::Error::other)??;
        Ok(AccountSessionsLock { file: locked })
    }

    pub(super) fn path(&self) -> PathBuf {
        self.config
            .codex_home
            .join(ACCOUNT_SESSIONS_FILE)
            .to_path_buf()
    }

    pub(super) fn session_home(&self, session_id: &str) -> PathBuf {
        self.config
            .codex_home
            .join(ACCOUNT_SESSIONS_CREDENTIALS_DIR)
            .join(session_id)
            .to_path_buf()
    }
}
