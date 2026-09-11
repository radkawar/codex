use super::*;

impl AccountSessionsStore<'_> {
    pub(super) fn resolve_session_id(
        stored: &StoredAccountSessions,
        selector: &str,
    ) -> std::io::Result<String> {
        if let Some(session) = stored
            .sessions
            .iter()
            .find(|session| session.session_id == selector)
        {
            return Ok(session.session_id.clone());
        }
        let mut matches = stored.sessions.iter().filter(|session| {
            session
                .email
                .as_deref()
                .is_some_and(|email| email.eq_ignore_ascii_case(selector))
        });
        let selected = matches.next().ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::NotFound,
                format!("Saved account {selector:?} not found; use /account to list accounts"),
            )
        })?;
        if matches.next().is_some() {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!("More than one saved account matches {selector:?}; select its account ID"),
            ));
        }
        Ok(selected.session_id.clone())
    }
}
