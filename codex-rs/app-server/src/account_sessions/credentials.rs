use super::*;

#[derive(Clone)]
pub(crate) struct AccountSessionCredentialsSnapshot {
    pub(crate) session_id: String,
    pub(crate) auth: AuthDotJson,
}

impl AccountSessionsStore<'_> {
    pub(crate) async fn snapshot_session_credentials(
        &self,
        session_id: &str,
    ) -> std::io::Result<AccountSessionCredentialsSnapshot> {
        let _lock = self.acquire_lock().await?;
        let mut stored = self.load_unlocked().await?;
        self.sync_active_auth_unlocked(&mut stored)?;
        let session_id = Self::resolve_session_id(&stored, session_id)?;
        let auth = self
            .load_session_auth(&session_id)?
            .ok_or_else(|| std::io::Error::other("Saved account session has no auth"))?;
        self.save_unlocked(&stored)?;
        Ok(AccountSessionCredentialsSnapshot { session_id, auth })
    }

    pub(crate) async fn persist_session_credentials_if_unchanged(
        &self,
        snapshot: &AccountSessionCredentialsSnapshot,
        refreshed_auth: &AuthDotJson,
    ) -> std::io::Result<bool> {
        let _lock = self.acquire_lock().await?;
        let mut stored = self.load_unlocked().await?;
        self.sync_active_auth_unlocked(&mut stored)?;
        let session = stored
            .sessions
            .iter_mut()
            .find(|session| session.session_id == snapshot.session_id);
        let Some(session) = session else {
            self.save_unlocked(&stored)?;
            return Ok(false);
        };
        if self.load_session_auth(&snapshot.session_id)?.as_ref() != Some(&snapshot.auth) {
            self.save_unlocked(&stored)?;
            return Ok(false);
        }
        self.save_session_auth(&snapshot.session_id, refreshed_auth)?;
        let refreshed_session = Self::session_from_auth_json(refreshed_auth);
        session.email = refreshed_session.email;
        session.user_id = refreshed_session.user_id;
        session.selected_workspace_account_id = refreshed_session.selected_workspace_account_id;
        if stored.active_session_id.as_ref() == Some(&snapshot.session_id)
            && self.load_active_auth()?.as_ref() == Some(&snapshot.auth)
        {
            self.save_session_as_active(&snapshot.session_id)?;
            self.auth_manager.reload().await;
        }
        self.save_unlocked(&stored)?;
        Ok(true)
    }
}
