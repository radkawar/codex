//! Compatibility RPCs and usage reads over the single saved-account store.

use super::*;
use crate::account_session_auth::IsolatedAccountSessionAuth;
use crate::auth_profile_rotation::select_next_auth_profile;
use codex_app_server_protocol::AccountSession;
use codex_app_server_protocol::AccountSessionsResponse;
use codex_protocol::protocol::RateLimitSnapshot as CoreRateLimitSnapshot;
use futures::StreamExt;

const ACCOUNT_RATE_LIMIT_CACHE_TTL: Duration = Duration::from_secs(60);
const ACCOUNT_RATE_LIMIT_TIMEOUT: Duration = Duration::from_secs(10);

pub(super) struct CachedAccountRateLimits {
    fetched_at: Instant,
    auth: AuthDotJson,
    snapshot: CoreRateLimitSnapshot,
}

impl AccountRequestProcessor {
    pub(super) async fn list_accounts_with_rate_limits(
        &self,
        refresh_workspace_metadata: bool,
    ) -> std::io::Result<AccountSessionsResponse> {
        let response = self
            .account_sessions_store()
            .list(refresh_workspace_metadata)
            .await?;
        Ok(self.with_account_rate_limits(response).await)
    }

    pub(super) async fn with_account_rate_limits(
        &self,
        mut response: AccountSessionsResponse,
    ) -> AccountSessionsResponse {
        self.account_rate_limit_cache
            .lock()
            .await
            .retain(|session_id, _| {
                response
                    .sessions
                    .iter()
                    .any(|session| &session.session_id == session_id)
            });
        response.sessions = futures::stream::iter(response.sessions)
            .map(|mut session| async move {
                session.rate_limits = self
                    .account_rate_limits(&session.session_id)
                    .await
                    .map(Into::into);
                if let Some(Account::Chatgpt { plan_type, .. }) = &mut session.account
                    && let Some(current_plan) = session
                        .rate_limits
                        .as_ref()
                        .and_then(|limits| limits.plan_type)
                {
                    *plan_type = current_plan;
                }
                session
            })
            .buffered(4)
            .collect()
            .await;
        response
    }

    async fn account_rate_limits(&self, session_id: &str) -> Option<CoreRateLimitSnapshot> {
        let store = self.account_sessions_store();
        let snapshot = store.snapshot_session_credentials(session_id).await.ok()?;
        {
            let cache = self.account_rate_limit_cache.lock().await;
            if let Some(cached) = cache.get(session_id)
                && cached.auth == snapshot.auth
                && cached.fetched_at.elapsed() <= ACCOUNT_RATE_LIMIT_CACHE_TTL
            {
                return Some(cached.snapshot.clone());
            }
        }

        let isolated = tokio::time::timeout(
            ACCOUNT_RATE_LIMIT_TIMEOUT,
            IsolatedAccountSessionAuth::new(&self.config, &store, session_id),
        )
        .await
        .ok()?
        .ok()?;
        let initial_auth = isolated.initial_auth().clone();
        let result = isolated
            .fetch_rate_limits(
                &self.config,
                ACCOUNT_RATE_LIMIT_TIMEOUT,
                &CancellationToken::new(),
            )
            .await
            .ok();
        if let Err(err) = isolated.persist_if_changed(&store).await {
            tracing::warn!("failed to persist saved account credential refresh: {err}");
        }
        let result = result?;
        self.account_rate_limit_cache.lock().await.insert(
            session_id.to_owned(),
            CachedAccountRateLimits {
                fetched_at: Instant::now(),
                auth: initial_auth,
                snapshot: result.clone(),
            },
        );
        Some(result)
    }

    pub(super) async fn list_auth_profiles_response(
        &self,
    ) -> Result<AuthProfileListResponse, JSONRPCErrorError> {
        let response = self
            .list_accounts_with_rate_limits(/*refresh_workspace_metadata*/ false)
            .await
            .map_err(|err| internal_error(format!("failed to list saved accounts: {err}")))?;
        Ok(AuthProfileListResponse {
            profiles: response.sessions.into_iter().map(account_profile).collect(),
        })
    }

    pub(super) async fn save_auth_profile_response(
        &self,
        params: AuthProfileSaveParams,
    ) -> Result<AuthProfileSaveResponse, JSONRPCErrorError> {
        let _ = params;
        Err(invalid_request(
            "Named auth profiles have been removed. Use accountSession/login/start to add an account, then select it by ID or email.",
        ))
    }

    pub(super) async fn activate_next_auth_profile_response(
        &self,
    ) -> Result<AuthProfileActivateNextResponse, JSONRPCErrorError> {
        let profiles = self.list_auth_profiles_response().await?.profiles;
        let profile = select_next_auth_profile(&profiles).ok_or_else(|| {
            invalid_request("no alternative saved account with available ChatGPT capacity")
        })?;
        let response = self.activate_auth_profile_by_name(&profile.name).await?;
        Ok(AuthProfileActivateNextResponse {
            profile: response.profile,
            current_account: response.current_account,
        })
    }

    pub(super) async fn activate_auth_profile_by_name(
        &self,
        name: &str,
    ) -> Result<AuthProfileActivateResponse, JSONRPCErrorError> {
        let response = self
            .account_sessions_store()
            .switch(name, /*account_id*/ None)
            .await
            .map_err(account_selection_error)?;
        self.sync_auth_after_account_session_change().await;
        let session = response
            .sessions
            .into_iter()
            .find(|session| session.is_active)
            .ok_or_else(|| internal_error("activated saved account disappeared unexpectedly"))?;
        let mut profile = account_profile(session);
        profile.rate_limits = self
            .account_rate_limits(&profile.name)
            .await
            .map(Into::into);
        Ok(AuthProfileActivateResponse {
            current_account: profile.account.clone(),
            profile,
        })
    }

    pub(super) async fn delete_auth_profile_response(
        &self,
        params: AuthProfileDeleteParams,
    ) -> Result<AuthProfileDeleteResponse, JSONRPCErrorError> {
        let _ = params;
        Err(invalid_request(
            "Named auth profiles have been removed. Use accountSession/logout with the saved account ID or email.",
        ))
    }
}

fn account_profile(session: AccountSession) -> AuthProfileSummary {
    AuthProfileSummary {
        name: session.session_id,
        account: session.account,
        rate_limits: session.rate_limits,
        active: session.is_active,
    }
}

fn account_selection_error(err: std::io::Error) -> JSONRPCErrorError {
    match err.kind() {
        std::io::ErrorKind::InvalidInput => invalid_params(err.to_string()),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists => {
            invalid_request(err.to_string())
        }
        _ => internal_error(format!("saved account operation failed: {err}")),
    }
}
