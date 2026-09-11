mod credentials;
mod migration;
mod selection;
mod support;

pub(crate) use credentials::AccountSessionCredentialsSnapshot;

#[cfg(test)]
#[path = "account_sessions_tests.rs"]
mod tests;

use chrono::Utc;
use codex_app_server_protocol::AccountSession;
use codex_app_server_protocol::AccountSessionWorkspace;
use codex_app_server_protocol::AccountSessionWorkspaceKind;
use codex_app_server_protocol::AccountSessionsResponse;
use codex_backend_client::Client as BackendClient;
use codex_core::config::Config;
use codex_login::AuthDotJson;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::load_auth_dot_json;
use codex_login::save_auth;
use serde::Deserialize;
use serde::Serialize;
use std::fs::File;
use std::io::ErrorKind;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tempfile::NamedTempFile;

const ACCOUNT_SESSIONS_FILE: &str = "account-sessions.json";
const ACCOUNT_SESSIONS_LOCK_FILE: &str = "account-sessions.lock";
const ACCOUNT_SESSIONS_CREDENTIALS_DIR: &str = "account-sessions";
const ACCOUNT_SESSIONS_SCHEMA_VERSION: u32 = 1;
const ACCOUNT_SESSIONS_LOCK_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct AccountSessionsStore<'a> {
    config: &'a Config,
    auth_manager: &'a Arc<AuthManager>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountSessionActivation {
    Activate,
    PreserveCurrent,
    RestorePrevious,
}
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredAccountSessions {
    schema_version: u32,
    #[serde(default)]
    legacy_profiles_migrated: bool,
    active_session_id: Option<String>,
    sessions: Vec<StoredAccountSession>,
}

impl Default for StoredAccountSessions {
    fn default() -> Self {
        Self {
            schema_version: ACCOUNT_SESSIONS_SCHEMA_VERSION,
            legacy_profiles_migrated: false,
            active_session_id: None,
            sessions: Vec::new(),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredAccountSession {
    session_id: String,
    email: Option<String>,
    user_id: Option<String>,
    display_name: Option<String>,
    image_url: Option<String>,
    last_used_at: i64,
    selected_workspace_account_id: Option<String>,
    workspaces: Vec<AccountSessionWorkspace>,
}

struct AccountSessionsLock {
    file: File,
}

impl Drop for AccountSessionsLock {
    fn drop(&mut self) {
        if let Err(err) = File::unlock(&self.file) {
            tracing::warn!("failed to unlock account session storage: {err}");
        }
    }
}

impl<'a> AccountSessionsStore<'a> {
    pub(crate) fn new(config: &'a Config, auth_manager: &'a Arc<AuthManager>) -> Self {
        Self {
            config,
            auth_manager,
        }
    }

    pub(crate) async fn add(
        &self,
        switch_to_added_account: bool,
    ) -> std::io::Result<AccountSessionsResponse> {
        let auth = self
            .auth_manager
            .auth()
            .await
            .ok_or_else(|| std::io::Error::other("No active ChatGPT auth session to add"))?;
        if !matches!(auth, CodexAuth::Chatgpt(_)) {
            return Err(std::io::Error::other(
                "Only Codex-managed ChatGPT logins can be saved as account sessions",
            ));
        }
        let auth_json = self
            .load_active_auth()?
            .filter(Self::is_managed_chatgpt_auth_json)
            .ok_or_else(|| std::io::Error::other("No active ChatGPT auth session to add"))?;
        let activation = if switch_to_added_account {
            AccountSessionActivation::Activate
        } else {
            AccountSessionActivation::RestorePrevious
        };
        self.add_auth(&auth_json, activation).await
    }

    pub(crate) async fn add_auth(
        &self,
        auth_json: &AuthDotJson,
        activation: AccountSessionActivation,
    ) -> std::io::Result<AccountSessionsResponse> {
        if !Self::is_managed_chatgpt_auth_json(auth_json) {
            return Err(std::io::Error::other(
                "Only managed ChatGPT credentials can be saved as account sessions",
            ));
        }
        let _lock = self.acquire_lock().await?;
        let previous_active_session_id = self
            .read_unlocked()?
            .and_then(|stored| stored.active_session_id);
        let mut stored = self.load_unlocked().await?;
        self.sync_active_auth_unlocked(&mut stored)?;
        let mut session = Self::session_from_auth_json(auth_json);
        let existing_index = self.find_matching_session(&stored, auth_json)?;
        if let Some(index) = existing_index {
            session
                .session_id
                .clone_from(&stored.sessions[index].session_id);
        }
        let added_session_id = session.session_id.clone();
        self.save_session_auth(&added_session_id, auth_json)?;
        self.refresh_workspace_metadata(&mut session).await;
        if let Some(index) = existing_index {
            stored.sessions[index] = session;
        } else {
            stored.sessions.push(session);
        }

        if activation == AccountSessionActivation::RestorePrevious
            && previous_active_session_id.is_some()
        {
            stored.active_session_id = previous_active_session_id;
        }
        let activate_added_account = match activation {
            AccountSessionActivation::Activate => true,
            AccountSessionActivation::RestorePrevious => stored.active_session_id.is_none(),
            AccountSessionActivation::PreserveCurrent => {
                stored.active_session_id.is_none() && self.auth_manager.auth_cached().is_none()
            }
        };
        if activate_added_account {
            self.save_session_as_active(&added_session_id)?;
            stored.active_session_id = Some(added_session_id);
        } else if activation == AccountSessionActivation::RestorePrevious
            && let Some(active_session_id) = stored.active_session_id.as_deref()
        {
            self.save_session_as_active(active_session_id)?;
        }

        self.save_unlocked(&stored)?;
        self.response(stored)
    }

    pub(crate) async fn list(
        &self,
        refresh_workspace_metadata: bool,
    ) -> std::io::Result<AccountSessionsResponse> {
        let _lock = self.acquire_lock().await?;
        let mut stored = self.load_unlocked().await?;
        self.sync_active_auth_unlocked(&mut stored)?;
        if refresh_workspace_metadata {
            for session in &mut stored.sessions {
                self.refresh_workspace_metadata(session).await;
            }
        }
        self.save_unlocked(&stored)?;
        self.response(stored)
    }

    pub(crate) async fn logout(
        &self,
        session_id: &str,
    ) -> std::io::Result<AccountSessionsResponse> {
        let _lock = self.acquire_lock().await?;
        let mut stored = self.load_unlocked().await?;
        self.sync_active_auth_unlocked(&mut stored)?;
        let session_id = Self::resolve_session_id(&stored, session_id)?;
        let session_id = session_id.as_str();
        let index = stored
            .sessions
            .iter()
            .position(|session| session.session_id == session_id)
            .ok_or_else(|| std::io::Error::other("Saved account session not found"))?;
        let removed = stored.sessions.remove(index);
        if let Err(err) = self
            .revoke_and_delete_session_auth(&removed.session_id)
            .await
        {
            tracing::warn!("failed to revoke saved account session during logout: {err}");
        }

        if stored.active_session_id.as_deref() == Some(session_id) {
            let newest = stored
                .sessions
                .iter()
                .max_by_key(|session| session.last_used_at);
            stored.active_session_id = newest.map(|session| session.session_id.clone());
            match newest {
                Some(session) => self.save_session_as_active(&session.session_id)?,
                None => {
                    self.auth_manager.logout().await?;
                }
            }
        }

        self.save_unlocked(&stored)?;
        self.response(stored)
    }

    pub(crate) async fn switch(
        &self,
        session_id: &str,
        account_id: Option<&str>,
    ) -> std::io::Result<AccountSessionsResponse> {
        let _lock = self.acquire_lock().await?;
        let mut stored = self.load_unlocked().await?;
        self.sync_active_auth_unlocked(&mut stored)?;
        let session_id = Self::resolve_session_id(&stored, session_id)?;
        let session_id = session_id.as_str();
        let index = stored
            .sessions
            .iter()
            .position(|session| session.session_id == session_id)
            .ok_or_else(|| std::io::Error::other("Saved account session not found"))?;
        let manager = self
            .session_auth_manager(self.session_home(session_id))
            .await;
        let auth = manager.auth().await.ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::PermissionDenied,
                "Saved account session has no permitted auth",
            )
        })?;
        if !self.config.auth_config().allows_auth(&auth) {
            return Err(std::io::Error::new(
                ErrorKind::PermissionDenied,
                "Saved account session is not allowed by the active authentication policy",
            ));
        }
        if let Some(account_id) = account_id {
            if !matches!(auth, CodexAuth::Chatgpt(_)) {
                return Err(std::io::Error::other(
                    "Workspace switching requires a Codex-managed ChatGPT login",
                ));
            }
            let token_data = auth.get_token_data()?;
            let mut client = BackendClient::from_auth(
                self.config.chatgpt_base_url.clone(),
                &auth,
                self.config.http_client_factory(),
            );
            if let Some(current_account_id) = token_data.account_id.as_ref() {
                client = client.with_chatgpt_account_id(current_account_id);
            }
            let accounts = client
                .get_accounts_check()
                .await
                .map_err(std::io::Error::other)?;
            if !accounts
                .accounts
                .iter()
                .any(|account| account.id == account_id)
            {
                return Err(std::io::Error::other(format!(
                    "Workspace {account_id:?} is not available to this ChatGPT account"
                )));
            }
            if let Some(allowed_workspaces) = self.auth_manager.effective_chatgpt_workspaces()
                && !allowed_workspaces.contains(&account_id.to_string())
            {
                return Err(std::io::Error::other(format!(
                    "Workspace {account_id:?} is not allowed by the active authentication policy"
                )));
            }

            let replacement = client
                .switch_workspace_token(account_id)
                .await
                .map_err(std::io::Error::other)?;
            let mut auth_json = self.load_session_auth(session_id)?.ok_or_else(|| {
                std::io::Error::other("Saved ChatGPT account session has no tokens")
            })?;
            let tokens = auth_json.tokens.as_mut().ok_or_else(|| {
                std::io::Error::other("Saved ChatGPT account session has no tokens")
            })?;
            tokens.access_token = replacement.access_token;
            if let Some(refresh_token) = replacement.refresh_token {
                tokens.refresh_token = refresh_token;
            }
            tokens.account_id = Some(account_id.to_string());
            auth_json.last_refresh = Some(Utc::now());
            self.save_session_auth(session_id, &auth_json)?;
            self.save_session_as_active(session_id)?;

            let session = &mut stored.sessions[index];
            session.selected_workspace_account_id = Some(account_id.to_string());
            session.workspaces = accounts
                .accounts
                .into_iter()
                .map(Self::workspace_from_account)
                .collect();
        } else {
            self.save_session_as_active(session_id)?;
        }

        let session = &mut stored.sessions[index];
        session.last_used_at = Utc::now().timestamp();
        stored.active_session_id = Some(session_id.to_string());
        self.save_unlocked(&stored)?;
        self.response(stored)
    }

    pub(crate) async fn sync_active_auth(&self) -> std::io::Result<()> {
        let _lock = self.acquire_lock().await?;
        let mut stored = self.load_unlocked().await?;
        self.sync_active_auth_unlocked(&mut stored)?;
        self.save_unlocked(&stored)
    }

    pub(crate) async fn deactivate(&self) -> std::io::Result<()> {
        let _lock = self.acquire_lock().await?;
        let Some(mut stored) = self.read_unlocked()? else {
            return Ok(());
        };
        self.sync_active_auth_unlocked(&mut stored)?;
        stored.active_session_id = None;
        self.save_unlocked(&stored)
    }

    pub(crate) async fn revoke_all_and_clear(&self) -> std::io::Result<bool> {
        let _lock = self.acquire_lock().await?;
        let stored = match self.load_unlocked().await {
            Ok(stored) => stored,
            Err(err) => {
                tracing::warn!("failed to read saved accounts during logout: {err}");
                StoredAccountSessions::default()
            }
        };
        let had_sessions = !stored.sessions.is_empty();
        for session in stored.sessions {
            if let Err(err) = self
                .revoke_and_delete_session_auth(&session.session_id)
                .await
            {
                tracing::warn!("failed to revoke saved account session during logout: {err}");
            }
        }
        self.save_unlocked(&StoredAccountSessions {
            legacy_profiles_migrated: true,
            ..StoredAccountSessions::default()
        })?;
        Ok(had_sessions)
    }

    async fn load_unlocked(&self) -> std::io::Result<StoredAccountSessions> {
        let mut stored = self.read_unlocked()?.unwrap_or_default();
        if !stored.legacy_profiles_migrated {
            self.sync_active_auth_unlocked(&mut stored)?;
            self.migrate_legacy_profiles_unlocked(&mut stored)?;
            self.save_unlocked(&stored)?;
        }
        Ok(stored)
    }

    fn read_unlocked(&self) -> std::io::Result<Option<StoredAccountSessions>> {
        let payload = match std::fs::read_to_string(self.path()) {
            Ok(payload) => payload,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err),
        };
        let stored: StoredAccountSessions = serde_json::from_str(&payload)?;
        if stored.schema_version != ACCOUNT_SESSIONS_SCHEMA_VERSION {
            return Err(std::io::Error::other(format!(
                "unsupported account session schema version {}",
                stored.schema_version
            )));
        }
        let mut session_ids = std::collections::HashSet::new();
        for session in &stored.sessions {
            if uuid::Uuid::parse_str(&session.session_id).is_err()
                || !session_ids.insert(&session.session_id)
            {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "saved account metadata contains an invalid or duplicate session ID",
                ));
            }
        }
        if stored.active_session_id.as_ref().is_some_and(|active| {
            !stored
                .sessions
                .iter()
                .any(|session| &session.session_id == active)
        }) {
            return Err(std::io::Error::other(
                "active account session is missing from account session metadata",
            ));
        }
        Ok(Some(stored))
    }

    fn save_unlocked(&self, sessions: &StoredAccountSessions) -> std::io::Result<()> {
        let path = self.path();
        let parent = path.parent().ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidInput,
                format!("path {} has no parent directory", path.display()),
            )
        })?;
        std::fs::create_dir_all(parent)?;
        let mut payload = serde_json::to_vec_pretty(sessions)?;
        payload.push(b'\n');
        let mut temporary = NamedTempFile::new_in(parent)?;
        temporary.write_all(&payload)?;
        temporary.as_file_mut().flush()?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(path)
            .map_err(|err| std::io::Error::other(err.error))?;
        Ok(())
    }

    fn sync_active_auth_unlocked(&self, stored: &mut StoredAccountSessions) -> std::io::Result<()> {
        let Some(auth_json) = self.load_active_auth()? else {
            stored.active_session_id = None;
            return Ok(());
        };
        let Some(index) = self.find_matching_session(stored, &auth_json)? else {
            stored.active_session_id = None;
            return Ok(());
        };
        let session = &mut stored.sessions[index];
        stored.active_session_id = Some(session.session_id.clone());
        self.save_session_auth(&session.session_id, &auth_json)?;
        if let Some(tokens) = auth_json.tokens.as_ref() {
            session.email.clone_from(&tokens.id_token.email);
            session.user_id.clone_from(&tokens.id_token.chatgpt_user_id);
        }
        if let Some(account_id) = auth_json
            .tokens
            .as_ref()
            .and_then(|tokens| tokens.account_id.clone())
        {
            session.selected_workspace_account_id = Some(account_id);
        }
        Ok(())
    }
}
