use super::*;
use crate::auth_profiles::load_auth_profile;

impl AccountSessionsStore<'_> {
    pub(super) fn migrate_legacy_profiles_unlocked(
        &self,
        stored: &mut StoredAccountSessions,
    ) -> std::io::Result<()> {
        // Existing canonical slots and the current login take precedence over
        // archived profiles, whose refresh tokens may already have been rotated.
        if let Some(auth) = self.load_active_auth()? {
            let index = match self.find_matching_session(stored, &auth)? {
                Some(index) => index,
                None => {
                    let session = Self::session_from_auth_json(&auth);
                    self.save_session_auth(&session.session_id, &auth)?;
                    stored.sessions.push(session);
                    stored.sessions.len() - 1
                }
            };
            stored.active_session_id = Some(stored.sessions[index].session_id.clone());
        }

        let mut profiles = Vec::new();
        match std::fs::read_dir(self.config.codex_home.join("accounts")) {
            Ok(entries) => {
                for entry in entries {
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(err) => {
                            tracing::warn!("failed to read legacy account profile entry: {err}");
                            continue;
                        }
                    };
                    match entry.file_type() {
                        Ok(file_type) if file_type.is_dir() => {}
                        Ok(_) => continue,
                        Err(err) => {
                            tracing::warn!("failed to inspect legacy account profile: {err}");
                            continue;
                        }
                    }
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    match load_auth_profile(&self.config.codex_home, name) {
                        Ok(auth) => profiles.push(auth),
                        Err(err) => {
                            tracing::warn!(
                                "leaving unreadable legacy account profile {name:?} in place: {err}"
                            );
                        }
                    }
                }
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => {
                tracing::warn!(
                    "leaving unreadable legacy account profile directory in place: {err}"
                );
            }
        }
        // Import the freshest legacy copy first; later duplicates cannot replace it.
        profiles.sort_by_key(|auth| std::cmp::Reverse(auth.last_refresh));
        for auth in profiles {
            if self.find_matching_session(stored, &auth)?.is_some() {
                continue;
            }
            let session = Self::session_from_auth_json(&auth);
            self.save_session_auth(&session.session_id, &auth)?;
            stored.sessions.push(session);
        }
        // Retain this marker even when all sessions are removed. The old files
        // remain a migration source only, never a second live credential store.
        stored.legacy_profiles_migrated = true;
        Ok(())
    }
}
