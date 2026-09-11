use super::bedrock_auth::BedrockProviderConfig;
use super::bedrock_auth::clear_user_model_provider_if_bedrock;
use super::bedrock_auth::configure_bedrock_provider;
use super::bedrock_auth::ensure_user_model_provider_can_be_bedrock;
use super::*;
use crate::account_priming::AccountPrimingController;
use crate::account_priming::DEFAULT_ACCOUNT_PRIMING_INTERVAL_SECONDS;
use crate::account_sessions::AccountSessionsStore;
use crate::auth_mode::auth_mode_to_api;
use crate::auth_profile_rotation::select_next_auth_profile;
use crate::auth_profiles::account_from_auth;
use crate::auth_profiles::auth_mode_from_auth;
use crate::auth_profiles::delete_auth_profile;
use crate::auth_profiles::list_auth_profiles;
use crate::auth_profiles::load_auth_profile;
use crate::auth_profiles::save_auth_profile;
use crate::external_auth::ExternalAuthBridge;
use chrono::DateTime;
use codex_app_server_protocol::AccountPrimingReadResponse;
use codex_app_server_protocol::AccountPrimingRunOnceResponse;
use codex_app_server_protocol::AccountPrimingStartParams;
use codex_app_server_protocol::AccountPrimingStartResponse;
use codex_app_server_protocol::AccountPrimingStopResponse;
use codex_app_server_protocol::AuthProfileActivateNextResponse;
use codex_app_server_protocol::AuthProfileActivateParams;
use codex_app_server_protocol::AuthProfileActivateResponse;
use codex_app_server_protocol::AuthProfileDeleteParams;
use codex_app_server_protocol::AuthProfileDeleteResponse;
use codex_app_server_protocol::AuthProfileListResponse;
use codex_app_server_protocol::AuthProfileSaveParams;
use codex_app_server_protocol::AuthProfileSaveResponse;
use codex_app_server_protocol::AuthProfileSummary;
use codex_app_server_protocol::DesktopOnboardingEntrypoint;
use codex_app_server_protocol::GetAccountRateLimitsParams;
use codex_login::LoginOnboardingEntrypoint;
use codex_login::login_with_bedrock_access_keys;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::load_auth_dot_json;
use codex_login::logout;
use codex_login::save_auth;
use codex_model_provider::BearerAuthProvider;
use codex_model_provider::is_supported_amazon_bedrock_region;
use codex_protocol::auth::AuthMode as CoreAuthMode;
use codex_protocol::protocol::RateLimitSnapshot as CoreRateLimitSnapshot;

mod bedrock_setup;
mod rate_limit_resets;

// Duration before a browser ChatGPT login attempt is abandoned.
const LOGIN_CHATGPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const ACCOUNT_TOKEN_USAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 10);
const THREAD_USAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(/*secs*/ 60);
const ACCOUNT_WORKSPACE_MESSAGES_FETCH_TIMEOUT: Duration =
    Duration::from_millis(/*millis*/ 1000);
// Packaged clients use this together with the OAuth client ID override for staging login.
const LOGIN_ISSUER_OVERRIDE_ENV_VAR: &str = "CODEX_APP_SERVER_LOGIN_ISSUER";
// The development success-page redirect remains debug-only.
#[cfg(debug_assertions)]
const LOGIN_OPEN_APP_URL_OVERRIDE_ENV_VAR: &str = "CODEX_APP_SERVER_DEV_OPEN_APP_URL";

enum ActiveLogin {
    Browser {
        shutdown_handle: ShutdownHandle,
        login_id: Uuid,
    },
    DeviceCode {
        cancel: CancellationToken,
        login_id: Uuid,
    },
}

impl ActiveLogin {
    fn login_id(&self) -> Uuid {
        match self {
            ActiveLogin::Browser { login_id, .. } | ActiveLogin::DeviceCode { login_id, .. } => {
                *login_id
            }
        }
    }

    fn cancel(&self) {
        match self {
            ActiveLogin::Browser {
                shutdown_handle, ..
            } => shutdown_handle.shutdown(),
            ActiveLogin::DeviceCode { cancel, .. } => cancel.cancel(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum CancelLoginError {
    NotFound,
}

enum RefreshTokenRequestOutcome {
    NotAttemptedOrSucceeded,
    FailedTransiently,
    FailedPermanently,
}

enum BedrockLoginCredentials {
    ApiKey(String),
    AccessKeys {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
}

#[derive(Clone, Debug)]
struct CachedAuthProfileRateLimits {
    fetched_at: Instant,
    snapshot: CoreRateLimitSnapshot,
}

const AUTH_PROFILE_RATE_LIMIT_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChatgptLoginDestination {
    Activate,
    AddWithoutSwitching,
}
impl Drop for ActiveLogin {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Clone)]
pub(crate) struct AccountRequestProcessor {
    auth_manager: Arc<AuthManager>,
    thread_manager: Arc<ThreadManager>,
    outgoing: Arc<OutgoingMessageSender>,
    config: Arc<Config>,
    config_manager: ConfigManager,
    active_login: Arc<Mutex<Option<ActiveLogin>>>,
    auth_profile_rate_limit_cache: Arc<Mutex<HashMap<String, CachedAuthProfileRateLimits>>>,
    account_priming: AccountPrimingController,
}

impl AccountRequestProcessor {
    pub(crate) fn new(
        auth_manager: Arc<AuthManager>,
        thread_manager: Arc<ThreadManager>,
        outgoing: Arc<OutgoingMessageSender>,
        config: Arc<Config>,
        config_manager: ConfigManager,
    ) -> Self {
        Self {
            auth_manager,
            thread_manager: Arc::clone(&thread_manager),
            outgoing,
            config: Arc::clone(&config),
            config_manager,
            active_login: Arc::new(Mutex::new(None)),
            auth_profile_rate_limit_cache: Arc::new(Mutex::new(HashMap::new())),
            account_priming: AccountPrimingController::new(config, thread_manager),
        }
    }

    pub(crate) async fn login_account(
        &self,
        request_id: ConnectionRequestId,
        params: LoginAccountParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.login_v2(request_id, params, ChatgptLoginDestination::Activate)
            .await
            .map(|()| None)
    }

    pub(crate) async fn login_account_session(
        &self,
        request_id: ConnectionRequestId,
        params: LoginAccountParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        if !matches!(
            params,
            LoginAccountParams::Chatgpt { .. } | LoginAccountParams::ChatgptDeviceCode
        ) {
            return Err(invalid_request(
                "account sessions support only Codex-managed ChatGPT login",
            ));
        }
        self.sync_active_account_session().await?;
        self.login_v2(
            request_id,
            params,
            ChatgptLoginDestination::AddWithoutSwitching,
        )
        .await
        .map(|()| None)
    }

    pub(crate) async fn logout_account(
        &self,
        request_id: ConnectionRequestId,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.logout_v2(request_id).await.map(|()| None)
    }

    pub(crate) async fn cancel_login_account(
        &self,
        params: CancelLoginAccountParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.cancel_login_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn add_account_session(
        &self,
        params: AccountSessionsAddParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let response = self
            .account_sessions_store()
            .add(params.switch_to_added_account)
            .await
            .map_err(|err| internal_error(format!("failed to add account session: {err}")))?;
        self.sync_auth_after_account_session_change().await;
        Ok(Some(response.into()))
    }

    pub(crate) async fn list_account_sessions(
        &self,
        params: AccountSessionsListParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.account_sessions_store()
            .list(params.refresh_workspace_metadata)
            .await
            .map(|response| Some(response.into()))
            .map_err(|err| internal_error(format!("failed to list account sessions: {err}")))
    }

    pub(crate) async fn logout_account_session(
        &self,
        params: AccountSessionsLogoutParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let response = self
            .account_sessions_store()
            .logout(&params.session_id)
            .await
            .map_err(|err| internal_error(format!("failed to log out account session: {err}")))?;
        self.sync_auth_after_account_session_change().await;
        Ok(Some(response.into()))
    }

    pub(crate) async fn switch_account_session(
        &self,
        params: AccountSessionsSwitchParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let response = self
            .account_sessions_store()
            .switch(&params.session_id, params.account_id.as_deref())
            .await
            .map_err(|err| internal_error(format!("failed to switch account session: {err}")))?;
        self.sync_auth_after_account_session_change().await;
        Ok(Some(response.into()))
    }

    pub(crate) async fn get_account(
        &self,
        params: GetAccountParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.get_account_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn get_auth_status(
        &self,
        params: GetAuthStatusParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.get_auth_status_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn get_account_rate_limits(
        &self,
        params: Option<GetAccountRateLimitsParams>,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.get_account_rate_limits_response(params.unwrap_or_default())
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn list_auth_profiles(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.list_auth_profiles_response()
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn save_auth_profile(
        &self,
        params: AuthProfileSaveParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.save_auth_profile_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn activate_auth_profile(
        &self,
        params: AuthProfileActivateParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let response = self.activate_auth_profile_by_name(&params.name).await?;
        self.outgoing
            .send_server_notification(ServerNotification::AccountUpdated(
                self.current_account_updated_notification(),
            ))
            .await;
        Ok(Some(response.into()))
    }

    pub(crate) async fn activate_next_auth_profile(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let response = self.activate_next_auth_profile_response().await?;
        self.outgoing
            .send_server_notification(ServerNotification::AccountUpdated(
                self.current_account_updated_notification(),
            ))
            .await;
        Ok(Some(response.into()))
    }

    pub(crate) async fn delete_auth_profile(
        &self,
        params: AuthProfileDeleteParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.delete_auth_profile_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn read_account_priming(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let status = self.account_priming.read_status().await;
        Ok(Some(AccountPrimingReadResponse { status }.into()))
    }

    pub(crate) async fn start_account_priming(
        &self,
        params: AccountPrimingStartParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let interval_seconds = params
            .interval_seconds
            .unwrap_or(DEFAULT_ACCOUNT_PRIMING_INTERVAL_SECONDS);
        if interval_seconds == 0 {
            return Err(invalid_params(
                "account priming interval_seconds must be at least 1",
            ));
        }

        self.account_priming
            .start(interval_seconds)
            .await
            .map(|status| Some(AccountPrimingStartResponse { status }.into()))
            .map_err(|err| invalid_request(format!("failed to start account priming: {err}")))
    }

    pub(crate) async fn stop_account_priming(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        let status = self.account_priming.stop().await;
        Ok(Some(AccountPrimingStopResponse { status }.into()))
    }

    pub(crate) async fn run_account_priming_once(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.account_priming
            .run_once()
            .await
            .map(|summary| Some(AccountPrimingRunOnceResponse { summary }.into()))
            .map_err(|err| invalid_request(format!("failed to run account priming pass: {err}")))
    }

    pub(crate) async fn get_account_token_usage(
        &self,
        params: Option<GetAccountTokenUsageParams>,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.get_account_token_usage_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn get_workspace_messages(
        &self,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.get_workspace_messages_response()
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn send_add_credits_nudge_email(
        &self,
        params: SendAddCreditsNudgeEmailParams,
    ) -> Result<Option<ClientResponsePayload>, JSONRPCErrorError> {
        self.send_add_credits_nudge_email_response(params)
            .await
            .map(|response| Some(response.into()))
    }

    pub(crate) async fn cancel_active_login(&self) {
        let mut guard = self.active_login.lock().await;
        if let Some(active_login) = guard.take() {
            drop(active_login);
        }
    }

    pub(crate) async fn shutdown_account_priming(&self) {
        self.account_priming.shutdown().await;
    }

    pub(crate) fn clear_external_auth(&self) {
        self.auth_manager.clear_external_auth();
    }

    fn current_account_updated_notification(&self) -> AccountUpdatedNotification {
        let (current_account, auth_mode) = self.current_account_and_auth_mode();
        AccountUpdatedNotification {
            auth_mode,
            plan_type: current_account.as_ref().and_then(|account| match account {
                Account::Chatgpt { plan_type, .. } => Some(*plan_type),
                Account::ApiKey {} | Account::AmazonBedrock { .. } => None,
            }),
            current_account,
        }
    }

    fn current_account_and_auth_mode(&self) -> (Option<Account>, Option<AuthMode>) {
        if let Ok(Some(auth)) = self.current_auth_dot_json() {
            return (account_from_auth(&auth), Some(auth_mode_from_auth(&auth)));
        }

        let auth = self.auth_manager.auth_cached();
        (
            auth.as_ref().and_then(Self::account_from_codex_auth),
            auth.as_ref()
                .map(CodexAuth::api_auth_mode)
                .map(auth_mode_to_api),
        )
    }

    fn account_sessions_store(&self) -> AccountSessionsStore<'_> {
        AccountSessionsStore::new(&self.config, &self.auth_manager)
    }

    async fn sync_active_account_session(&self) -> Result<(), JSONRPCErrorError> {
        self.account_sessions_store()
            .sync_active_auth()
            .await
            .map_err(|err| internal_error(format!("failed to sync active account session: {err}")))
    }

    async fn sync_auth_after_account_session_change(&self) {
        self.auth_manager.reload().await;
        self.config_manager.replace_cloud_config_bundle_loader(
            self.auth_manager.clone(),
            self.config.chatgpt_base_url.clone(),
            self.config.http_client_factory(),
        );
        self.config_manager
            .sync_default_client_residency_requirement()
            .await;
        Self::maybe_refresh_plugin_caches_for_current_config(
            &self.config_manager,
            &self.thread_manager,
            self.auth_manager.auth_cached(),
        )
        .await;
        self.outgoing
            .send_server_notification(ServerNotification::AccountUpdated(
                self.current_account_updated_notification(),
            ))
            .await;
    }

    async fn load_latest_config(&self) -> Config {
        match self
            .config_manager
            .load_latest_config(/*fallback_cwd*/ None)
            .await
        {
            Ok(config) => config,
            Err(err) => {
                tracing::warn!("failed to reload config, using startup config: {err}");
                self.config.as_ref().clone()
            }
        }
    }

    async fn maybe_refresh_plugin_caches_for_current_config(
        config_manager: &ConfigManager,
        thread_manager: &Arc<ThreadManager>,
        auth: Option<CodexAuth>,
    ) {
        thread_manager
            .plugins_manager()
            .clear_recommended_plugins_cache();

        match config_manager
            .load_latest_config(/*fallback_cwd*/ None)
            .await
        {
            Ok(config) => {
                Self::spawn_effective_plugins_changed_task(
                    Arc::clone(thread_manager),
                    config_manager.clone(),
                );
                let plugins_config = config.plugins_config_input();
                let refresh_thread_manager = Arc::clone(thread_manager);
                let refresh_config_manager = config_manager.clone();
                let on_effective_plugins_changed: Arc<
                    dyn Fn(codex_core_plugins::EffectivePluginsChange) + Send + Sync,
                > = Arc::new(move |_change| {
                    Self::spawn_effective_plugins_changed_task(
                        Arc::clone(&refresh_thread_manager),
                        refresh_config_manager.clone(),
                    );
                });
                thread_manager
                    .plugins_manager()
                    .maybe_start_curated_repo_sync_for_config(
                        &plugins_config,
                        Some(Arc::clone(&on_effective_plugins_changed)),
                    );
                thread_manager
                    .plugins_manager()
                    .maybe_start_remote_plugin_caches_refresh(
                        &plugins_config,
                        auth,
                        Some(on_effective_plugins_changed),
                    );
            }
            Err(err) => {
                warn!(
                    "failed to reload config after account changed, skipping remote installed plugins cache refresh: {err}"
                );
            }
        }
    }

    fn spawn_effective_plugins_changed_task(
        thread_manager: Arc<ThreadManager>,
        config_manager: ConfigManager,
    ) {
        tokio::spawn(async move {
            thread_manager.plugins_manager().clear_cache();
            thread_manager.skills_service().clear_cache();
            crate::mcp_refresh::reload_mcp_config_best_effort(&thread_manager, &config_manager)
                .await;
            thread_manager.invalidate_mcp_runtimes().await;
        });
    }

    async fn login_v2(
        &self,
        request_id: ConnectionRequestId,
        params: LoginAccountParams,
        chatgpt_destination: ChatgptLoginDestination,
    ) -> Result<(), JSONRPCErrorError> {
        if self.auth_manager.is_workload_identity_selected() {
            return Err(self.configured_auth_owned_by_host_error());
        }
        match params {
            LoginAccountParams::ApiKey { api_key } => {
                self.login_api_key_v2(request_id, LoginApiKeyParams { api_key })
                    .await;
            }
            LoginAccountParams::Chatgpt {
                app_brand,
                codex_streamlined_login,
                use_hosted_login_success_page,
            } => {
                let login_success_page = if use_hosted_login_success_page {
                    let app_brand = match app_brand.unwrap_or_default() {
                        LoginAppBrand::Codex => LoginSuccessPageBrand::Codex,
                        LoginAppBrand::Chatgpt => LoginSuccessPageBrand::Chatgpt,
                    };
                    LoginSuccessPage::Hosted {
                        url: CODEX_OPEN_APP_URL.parse().map_err(|err| {
                            internal_error(format!("invalid Codex open app URL: {err}"))
                        })?,
                        app_brand,
                    }
                } else {
                    LoginSuccessPage::default()
                };
                self.login_chatgpt_v2(
                    request_id,
                    codex_streamlined_login,
                    login_success_page,
                    chatgpt_destination,
                )
                .await;
            }
            LoginAccountParams::ChatgptDeviceCode => {
                self.login_chatgpt_device_code_v2(request_id, chatgpt_destination)
                    .await;
            }
            LoginAccountParams::ChatgptAuthTokens {
                access_token,
                chatgpt_account_id,
                chatgpt_plan_type,
            } => {
                self.login_chatgpt_auth_tokens(
                    request_id,
                    access_token,
                    chatgpt_account_id,
                    chatgpt_plan_type,
                )
                .await;
            }
            LoginAccountParams::AmazonBedrock { api_key, region } => {
                self.login_amazon_bedrock_v2(
                    request_id,
                    BedrockLoginCredentials::ApiKey(api_key),
                    region,
                )
                .await;
            }
            LoginAccountParams::AmazonBedrockAccessKeys {
                access_key_id,
                secret_access_key,
                session_token,
                region,
            } => {
                self.login_amazon_bedrock_v2(
                    request_id,
                    BedrockLoginCredentials::AccessKeys {
                        access_key_id,
                        secret_access_key,
                        session_token,
                    },
                    region,
                )
                .await;
            }
        }
        Ok(())
    }

    fn external_auth_active_error(&self) -> JSONRPCErrorError {
        invalid_request(
            "External auth is active. Use account/login/start (chatgptAuthTokens) to update it or account/logout to clear it.",
        )
    }

    fn configured_auth_owned_by_host_error(&self) -> JSONRPCErrorError {
        invalid_request(
            "Configured external authentication is owned by the app-server host and cannot be changed through account RPCs.",
        )
    }

    fn ensure_bedrock_login_allowed(&self) -> Result<(), JSONRPCErrorError> {
        if self.auth_manager.is_workload_identity_selected() {
            return Err(self.configured_auth_owned_by_host_error());
        }
        if self.auth_manager.is_external_chatgpt_auth_active() {
            return Err(self.external_auth_active_error());
        }
        if !self
            .auth_manager
            .is_login_method_allowed(ForcedLoginMethod::Api)
        {
            return Err(invalid_request(
                "Amazon Bedrock login is disabled. Use ChatGPT login instead.",
            ));
        }
        Ok(())
    }

    async fn login_api_key_common(
        &self,
        params: &LoginApiKeyParams,
    ) -> std::result::Result<(), JSONRPCErrorError> {
        if self.auth_manager.is_external_chatgpt_auth_active() {
            return Err(self.external_auth_active_error());
        }

        if !self
            .auth_manager
            .is_login_method_allowed(ForcedLoginMethod::Api)
        {
            return Err(invalid_request(
                "API key login is disabled. Use ChatGPT login instead.",
            ));
        }

        self.account_sessions_store()
            .deactivate()
            .await
            .map_err(|err| internal_error(format!("failed to save account sessions: {err}")))?;

        // Cancel any active login attempt.
        {
            let mut guard = self.active_login.lock().await;
            if let Some(active) = guard.take() {
                drop(active);
            }
        }

        match login_with_api_key(
            &self.config.codex_home,
            &params.api_key,
            self.config.cli_auth_credentials_store_mode,
            self.config.auth_keyring_backend_kind(),
        ) {
            Ok(()) => {
                self.auth_manager.reload().await;
                self.config_manager.clear_cloud_config_bundle_loader();
                Ok(())
            }
            Err(err) => Err(internal_error(format!("failed to save api key: {err}"))),
        }
    }

    async fn login_api_key_v2(&self, request_id: ConnectionRequestId, params: LoginApiKeyParams) {
        let result = self
            .login_api_key_common(&params)
            .await
            .map(|()| LoginAccountResponse::ApiKey {});
        let logged_in = result.is_ok();
        self.outgoing.send_result(request_id, result).await;

        if logged_in {
            self.send_login_success_notifications(/*login_id*/ None)
                .await;
        }
    }

    async fn login_amazon_bedrock_v2(
        &self,
        request_id: ConnectionRequestId,
        credentials: BedrockLoginCredentials,
        region: String,
    ) {
        let result = async {
            self.ensure_bedrock_login_allowed()?;

            match &credentials {
                BedrockLoginCredentials::ApiKey(api_key) => {
                    if api_key.trim().is_empty() {
                        return Err(invalid_request("Amazon Bedrock API key must not be empty."));
                    }
                }
                BedrockLoginCredentials::AccessKeys {
                    access_key_id,
                    secret_access_key,
                    ..
                } => {
                    if access_key_id.trim().is_empty() || secret_access_key.trim().is_empty() {
                        return Err(invalid_request(
                            "AWS access key ID and secret access key must not be empty.",
                        ));
                    }
                }
            }
            let region = region.trim();
            if !is_supported_amazon_bedrock_region(region) {
                return Err(invalid_request(format!(
                    "Amazon Bedrock does not support region `{region}`"
                )));
            }

            self.cancel_active_login().await;
            ensure_user_model_provider_can_be_bedrock(&self.config_manager).await?;
            self.account_sessions_store()
                .deactivate()
                .await
                .map_err(|err| internal_error(format!("failed to save account sessions: {err}")))?;

            configure_bedrock_provider(
                &self.config_manager,
                BedrockProviderConfig {
                    region: matches!(&credentials, BedrockLoginCredentials::AccessKeys { .. })
                        .then_some(region),
                    profile: None,
                },
            )
            .await?;

            match credentials {
                BedrockLoginCredentials::ApiKey(api_key) => login_with_bedrock_api_key(
                    &self.config.codex_home,
                    api_key.trim(),
                    region,
                    self.config.cli_auth_credentials_store_mode,
                    self.config.auth_keyring_backend_kind(),
                ),
                BedrockLoginCredentials::AccessKeys {
                    access_key_id,
                    secret_access_key,
                    session_token,
                } => {
                    let session_token = session_token
                        .as_deref()
                        .map(str::trim)
                        .filter(|token| !token.is_empty());
                    login_with_bedrock_access_keys(
                        &self.config.codex_home,
                        access_key_id.trim(),
                        secret_access_key.trim(),
                        session_token,
                        self.config.cli_auth_credentials_store_mode,
                        self.config.auth_keyring_backend_kind(),
                    )
                }
            }
            .map_err(|err| internal_error(format!("failed to save Amazon Bedrock auth: {err}")))?;
            self.auth_manager.reload().await;
            self.config_manager.clear_cloud_config_bundle_loader();
            Ok(LoginAccountResponse::AmazonBedrock {})
        }
        .await;
        let logged_in = result.is_ok();
        self.outgoing.send_result(request_id, result).await;

        if logged_in {
            self.send_login_success_notifications(/*login_id*/ None)
                .await;
        }
    }

    // Build options for a ChatGPT login attempt; performs validation.
    async fn login_chatgpt_common(
        &self,
        codex_streamlined_login: bool,
        login_success_page: LoginSuccessPage,
    ) -> std::result::Result<LoginServerOptions, JSONRPCErrorError> {
        let config = self.config.as_ref();

        if self.auth_manager.is_external_chatgpt_auth_active() {
            return Err(self.external_auth_active_error());
        }

        if !self
            .auth_manager
            .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
        {
            return Err(invalid_request(
                "ChatGPT login is disabled. Use API key login instead.",
            ));
        }

        let mut opts = LoginServerOptions {
            open_browser: false,
            codex_streamlined_login,
            login_success_page,
            ..LoginServerOptions::new(
                config.codex_home.to_path_buf(),
                oauth_client_id(),
                self.auth_manager.effective_chatgpt_workspaces(),
                config.cli_auth_credentials_store_mode,
                config.auth_keyring_backend_kind(),
                config.auth_route_config(),
            )
        };
        if let Ok(issuer) = std::env::var(LOGIN_ISSUER_OVERRIDE_ENV_VAR)
            && !issuer.trim().is_empty()
        {
            opts.issuer = issuer;
        }
        #[cfg(debug_assertions)]
        if let LoginSuccessPage::Hosted { url, .. } = &mut opts.login_success_page
            && let Ok(open_app_url) = std::env::var(LOGIN_OPEN_APP_URL_OVERRIDE_ENV_VAR)
            && !open_app_url.trim().is_empty()
        {
            *url = open_app_url
                .parse()
                .map_err(|err| internal_error(format!("invalid Codex open app URL: {err}")))?;
        }

        Ok(opts)
    }

    fn login_chatgpt_device_code_start_error(err: IoError) -> JSONRPCErrorError {
        let is_not_found = err.kind() == std::io::ErrorKind::NotFound;
        if is_not_found {
            invalid_request(err.to_string())
        } else {
            internal_error(format!("failed to request device code: {err}"))
        }
    }

    async fn login_chatgpt_v2(
        &self,
        request_id: ConnectionRequestId,
        codex_streamlined_login: bool,
        login_success_page: LoginSuccessPage,
        destination: ChatgptLoginDestination,
    ) {
        let result = self
            .login_chatgpt_response(codex_streamlined_login, login_success_page, destination)
            .await;
        self.outgoing.send_result(request_id, result).await;
    }

    async fn login_chatgpt_response(
        &self,
        codex_streamlined_login: bool,
        login_success_page: LoginSuccessPage,
        destination: ChatgptLoginDestination,
    ) -> Result<LoginAccountResponse, JSONRPCErrorError> {
        self.sync_active_account_session().await?;
        let opts = self
            .login_chatgpt_common(codex_streamlined_login, login_success_page)
            .await?;
        let server = run_login_server(opts)
            .map_err(|err| internal_error(format!("failed to start login server: {err}")))?;
        let login_id = Uuid::new_v4();
        let shutdown_handle = server.cancel_handle();

        // Replace active login if present.
        {
            let mut guard = self.active_login.lock().await;
            if let Some(existing) = guard.take() {
                drop(existing);
            }
            *guard = Some(ActiveLogin::Browser {
                shutdown_handle: shutdown_handle.clone(),
                login_id,
            });
        }

        let outgoing_clone = self.outgoing.clone();
        let config_manager = self.config_manager.clone();
        let thread_manager = Arc::clone(&self.thread_manager);
        let config = Arc::clone(&self.config);
        let active_login = self.active_login.clone();
        let auth_url = server.auth_url.clone();
        tokio::spawn(async move {
            let (success, error_msg, onboarding_entrypoint) = match tokio::time::timeout(
                LOGIN_CHATGPT_TIMEOUT,
                server.block_until_done_with_callback_result(),
            )
            .await
            {
                Ok(Ok(result)) => (
                    true,
                    None,
                    result
                        .onboarding_entrypoint
                        .map(|LoginOnboardingEntrypoint::LifeSciences| {
                            DesktopOnboardingEntrypoint::LifeSciences
                        }),
                ),
                Ok(Err(err)) => (false, Some(format!("Login server error: {err}")), None),
                Err(_elapsed) => {
                    shutdown_handle.shutdown();
                    (false, Some("Login timed out".to_string()), None)
                }
            };

            Self::send_chatgpt_login_completion_notifications(
                &outgoing_clone,
                config_manager,
                thread_manager,
                config,
                AccountLoginCompletedNotification {
                    login_id: Some(login_id.to_string()),
                    success,
                    error: error_msg,
                    onboarding_entrypoint,
                },
                destination,
            )
            .await;

            // Clear the active login if it matches this attempt. It may have been replaced or cancelled.
            let mut guard = active_login.lock().await;
            if guard.as_ref().map(ActiveLogin::login_id) == Some(login_id) {
                *guard = None;
            }
        });

        Ok(LoginAccountResponse::Chatgpt {
            login_id: login_id.to_string(),
            auth_url,
        })
    }

    async fn login_chatgpt_device_code_v2(
        &self,
        request_id: ConnectionRequestId,
        destination: ChatgptLoginDestination,
    ) {
        let result = self.login_chatgpt_device_code_response(destination).await;
        self.outgoing.send_result(request_id, result).await;
    }

    async fn login_chatgpt_device_code_response(
        &self,
        destination: ChatgptLoginDestination,
    ) -> Result<LoginAccountResponse, JSONRPCErrorError> {
        self.sync_active_account_session().await?;
        let opts = self
            .login_chatgpt_common(
                /*codex_streamlined_login*/ false,
                LoginSuccessPage::default(),
            )
            .await?;
        let device_code = request_device_code(&opts)
            .await
            .map_err(Self::login_chatgpt_device_code_start_error)?;
        let login_id = Uuid::new_v4();
        let cancel = CancellationToken::new();

        {
            let mut guard = self.active_login.lock().await;
            if let Some(existing) = guard.take() {
                drop(existing);
            }
            *guard = Some(ActiveLogin::DeviceCode {
                cancel: cancel.clone(),
                login_id,
            });
        }

        let verification_url = device_code.verification_url.clone();
        let user_code = device_code.user_code.clone();

        let outgoing_clone = self.outgoing.clone();
        let config_manager = self.config_manager.clone();
        let thread_manager = Arc::clone(&self.thread_manager);
        let config = Arc::clone(&self.config);
        let active_login = self.active_login.clone();
        tokio::spawn(async move {
            let (success, error_msg) = tokio::select! {
                _ = cancel.cancelled() => {
                    (false, Some("Login was not completed".to_string()))
                }
                r = complete_device_code_login(opts, device_code) => {
                    match r {
                        Ok(()) => (true, None),
                        Err(err) => (false, Some(err.to_string())),
                    }
                }
            };

            Self::send_chatgpt_login_completion_notifications(
                &outgoing_clone,
                config_manager,
                thread_manager,
                config,
                AccountLoginCompletedNotification {
                    login_id: Some(login_id.to_string()),
                    success,
                    error: error_msg,
                    onboarding_entrypoint: None,
                },
                destination,
            )
            .await;

            let mut guard = active_login.lock().await;
            if guard.as_ref().map(ActiveLogin::login_id) == Some(login_id) {
                *guard = None;
            }
        });

        Ok(LoginAccountResponse::ChatgptDeviceCode {
            login_id: login_id.to_string(),
            verification_url,
            user_code,
        })
    }

    async fn cancel_login_chatgpt_common(
        &self,
        login_id: Uuid,
    ) -> std::result::Result<(), CancelLoginError> {
        let mut guard = self.active_login.lock().await;
        if guard.as_ref().map(ActiveLogin::login_id) == Some(login_id) {
            if let Some(active) = guard.take() {
                drop(active);
            }
            Ok(())
        } else {
            Err(CancelLoginError::NotFound)
        }
    }

    async fn cancel_login_response(
        &self,
        params: CancelLoginAccountParams,
    ) -> Result<CancelLoginAccountResponse, JSONRPCErrorError> {
        let login_id = params.login_id;
        let uuid = Uuid::parse_str(&login_id)
            .map_err(|_| invalid_request(format!("invalid login id: {login_id}")))?;
        let status = match self.cancel_login_chatgpt_common(uuid).await {
            Ok(()) => CancelLoginAccountStatus::Canceled,
            Err(CancelLoginError::NotFound) => CancelLoginAccountStatus::NotFound,
        };
        Ok(CancelLoginAccountResponse { status })
    }

    async fn login_chatgpt_auth_tokens(
        &self,
        request_id: ConnectionRequestId,
        access_token: String,
        chatgpt_account_id: String,
        chatgpt_plan_type: Option<String>,
    ) {
        let result = self
            .login_chatgpt_auth_tokens_response(access_token, chatgpt_account_id, chatgpt_plan_type)
            .await;
        let logged_in = result.is_ok();
        self.outgoing.send_result(request_id, result).await;

        if logged_in {
            self.send_login_success_notifications(/*login_id*/ None)
                .await;
        }
    }

    async fn login_chatgpt_auth_tokens_response(
        &self,
        access_token: String,
        chatgpt_account_id: String,
        chatgpt_plan_type: Option<String>,
    ) -> Result<LoginAccountResponse, JSONRPCErrorError> {
        if !self
            .auth_manager
            .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
        {
            return Err(invalid_request(
                "External ChatGPT auth is disabled. Use API key login instead.",
            ));
        }

        // Cancel any active login attempt to avoid persisting managed auth state.
        {
            let mut guard = self.active_login.lock().await;
            if let Some(active) = guard.take() {
                drop(active);
            }
        }

        if let Some(expected_workspaces) = self.auth_manager.effective_chatgpt_workspaces()
            && !expected_workspaces.contains(&chatgpt_account_id)
        {
            return Err(invalid_request(format!(
                "External auth must use one of workspace(s) {expected_workspaces:?}, but received {chatgpt_account_id:?}.",
            )));
        }

        self.account_sessions_store()
            .deactivate()
            .await
            .map_err(|err| internal_error(format!("failed to save account sessions: {err}")))?;

        let auth = CodexAuth::from_external_chatgpt_tokens(
            &access_token,
            &chatgpt_account_id,
            chatgpt_plan_type.as_deref(),
        )
        .map_err(|err| internal_error(format!("failed to set external auth: {err}")))?;
        self.auth_manager
            .set_external_auth(Arc::new(ExternalAuthBridge::new(
                Arc::clone(&self.outgoing),
                auth,
            )))
            .await
            .map_err(|err| internal_error(format!("failed to set external auth: {err}")))?;
        self.config_manager.replace_cloud_config_bundle_loader(
            self.auth_manager.clone(),
            self.config.chatgpt_base_url.clone(),
            self.config.http_client_factory(),
        );
        self.config_manager
            .sync_default_client_residency_requirement()
            .await;

        Ok(LoginAccountResponse::ChatgptAuthTokens {})
    }

    async fn send_login_success_notifications(&self, login_id: Option<Uuid>) {
        Self::maybe_refresh_plugin_caches_for_current_config(
            &self.config_manager,
            &self.thread_manager,
            self.auth_manager.auth_cached(),
        )
        .await;

        let payload_login_completed = AccountLoginCompletedNotification {
            login_id: login_id.map(|id| id.to_string()),
            success: true,
            error: None,
            onboarding_entrypoint: None,
        };
        self.outgoing
            .send_server_notification(ServerNotification::AccountLoginCompleted(
                payload_login_completed,
            ))
            .await;

        self.outgoing
            .send_server_notification(ServerNotification::AccountUpdated(
                self.current_account_updated_notification(),
            ))
            .await;
    }

    async fn send_chatgpt_login_completion_notifications(
        outgoing: &OutgoingMessageSender,
        config_manager: ConfigManager,
        thread_manager: Arc<ThreadManager>,
        config: Arc<Config>,
        mut payload_v2: AccountLoginCompletedNotification,
        destination: ChatgptLoginDestination,
    ) {
        if payload_v2.success {
            let auth_manager = thread_manager.auth_manager();
            auth_manager.reload().await;
            let switch_to_added_account = destination == ChatgptLoginDestination::Activate;
            if let Err(err) = AccountSessionsStore::new(&config, &auth_manager)
                .add(switch_to_added_account)
                .await
            {
                payload_v2.success = false;
                payload_v2.error = Some(format!("failed to save account session: {err}"));
            }
        }
        let success = payload_v2.success;
        outgoing
            .send_server_notification(ServerNotification::AccountLoginCompleted(payload_v2))
            .await;

        if success {
            let auth_manager = thread_manager.auth_manager();
            auth_manager.reload().await;
            config_manager.replace_cloud_config_bundle_loader(
                auth_manager.clone(),
                config.chatgpt_base_url.clone(),
                config.http_client_factory(),
            );
            config_manager
                .sync_default_client_residency_requirement()
                .await;

            let auth = auth_manager.auth_cached();
            Self::maybe_refresh_plugin_caches_for_current_config(
                &config_manager,
                &thread_manager,
                auth.clone(),
            )
            .await;
            let payload_v2 = AccountUpdatedNotification {
                auth_mode: auth
                    .as_ref()
                    .map(CodexAuth::api_auth_mode)
                    .map(auth_mode_to_api),
                plan_type: auth.as_ref().and_then(CodexAuth::account_plan_type),
                current_account: auth.as_ref().and_then(Self::account_from_codex_auth),
            };
            outgoing
                .send_server_notification(ServerNotification::AccountUpdated(payload_v2))
                .await;
        }
    }

    async fn logout_common(&self) -> std::result::Result<Option<AuthMode>, JSONRPCErrorError> {
        if self.auth_manager.is_workload_identity_selected() {
            return Err(self.configured_auth_owned_by_host_error());
        }
        let config = self.load_latest_config().await;

        // Cancel any active login attempt.
        {
            let mut guard = self.active_login.lock().await;
            if let Some(active) = guard.take() {
                drop(active);
            }
        }

        let cleared_account_sessions = self
            .account_sessions_store()
            .revoke_all_and_clear()
            .await
            .map_err(|err| {
            internal_error(format!("failed to clear account sessions: {err}"))
        })?;
        let logout_result = if cleared_account_sessions {
            self.auth_manager.logout().await
        } else {
            self.auth_manager.logout_with_revoke().await
        };
        match logout_result {
            Ok(_) => {}
            Err(err) => {
                return Err(internal_error(format!("logout failed: {err}")));
            }
        }

        if config.model_provider.is_amazon_bedrock() {
            clear_user_model_provider_if_bedrock(&self.config_manager, &config).await?;
        }

        self.config_manager.clear_cloud_config_bundle_loader();

        Self::maybe_refresh_plugin_caches_for_current_config(
            &self.config_manager,
            &self.thread_manager,
            self.auth_manager.auth_cached(),
        )
        .await;

        // Reflect the current auth method after logout (likely None).
        Ok(self
            .auth_manager
            .auth_cached()
            .as_ref()
            .map(CodexAuth::api_auth_mode)
            .map(auth_mode_to_api))
    }

    async fn logout_v2(&self, request_id: ConnectionRequestId) -> Result<(), JSONRPCErrorError> {
        let result = self.logout_common().await;
        let account_updated =
            result
                .as_ref()
                .ok()
                .cloned()
                .map(|auth_mode| AccountUpdatedNotification {
                    auth_mode,
                    plan_type: None,
                    current_account: None,
                });
        self.outgoing
            .send_result(request_id, result.map(|_| LogoutAccountResponse {}))
            .await;

        if let Some(payload) = account_updated {
            self.outgoing
                .send_server_notification(ServerNotification::AccountUpdated(payload))
                .await;
        }
        Ok(())
    }

    async fn refresh_token_if_requested(&self, do_refresh: bool) -> RefreshTokenRequestOutcome {
        if self.auth_manager.is_external_chatgpt_auth_active() {
            return RefreshTokenRequestOutcome::NotAttemptedOrSucceeded;
        }
        if do_refresh && let Err(err) = self.auth_manager.refresh_token().await {
            let failed_reason = err.failed_reason();
            if failed_reason.is_none() {
                tracing::warn!("failed to refresh token while getting account: {err}");
                return RefreshTokenRequestOutcome::FailedTransiently;
            }
            return RefreshTokenRequestOutcome::FailedPermanently;
        }
        RefreshTokenRequestOutcome::NotAttemptedOrSucceeded
    }

    async fn get_auth_status_response(
        &self,
        params: GetAuthStatusParams,
    ) -> Result<GetAuthStatusResponse, JSONRPCErrorError> {
        let include_token = params.include_token.unwrap_or(false);
        let do_refresh = params.refresh_token.unwrap_or(false);

        self.refresh_token_if_requested(do_refresh).await;

        // Determine whether auth is required based on the active model provider.
        // If a custom provider is configured with `requires_openai_auth == false`,
        // then no auth step is required; otherwise, default to requiring auth.
        let config = self.load_latest_config().await;
        let requires_openai_auth = config.model_provider.requires_openai_auth;

        let response = if !requires_openai_auth {
            GetAuthStatusResponse {
                auth_method: None,
                auth_token: None,
                requires_openai_auth: Some(false),
            }
        } else {
            let auth = if do_refresh {
                self.auth_manager.auth_cached()
            } else {
                self.auth_manager.auth().await
            };
            match auth {
                Some(auth) => {
                    let permanent_refresh_failure =
                        self.auth_manager.refresh_failure_for_auth(&auth).is_some();
                    let auth_mode = auth_mode_to_api(auth.api_auth_mode());
                    let (reported_auth_method, token_opt) =
                        if self.auth_manager.is_workload_identity_selected()
                            || matches!(
                                auth,
                                CodexAuth::Headers(_)
                                    | CodexAuth::AgentIdentity(_)
                                    | CodexAuth::PersonalAccessToken(_)
                            )
                            || include_token && permanent_refresh_failure
                        {
                            // Host-owned and metadata-bearing credentials are never exported.
                            (Some(auth_mode), None)
                        } else {
                            match auth.get_token() {
                                Ok(token) if !token.is_empty() => {
                                    let tok = if include_token { Some(token) } else { None };
                                    (Some(auth_mode), tok)
                                }
                                Ok(_) => (None, None),
                                Err(err) => {
                                    tracing::warn!("failed to get token for auth status: {err}");
                                    (None, None)
                                }
                            }
                        };
                    GetAuthStatusResponse {
                        auth_method: reported_auth_method,
                        auth_token: token_opt,
                        requires_openai_auth: Some(true),
                    }
                }
                None => GetAuthStatusResponse {
                    auth_method: None,
                    auth_token: None,
                    requires_openai_auth: Some(true),
                },
            }
        };

        Ok(response)
    }

    async fn get_account_response(
        &self,
        params: GetAccountParams,
    ) -> Result<GetAccountResponse, JSONRPCErrorError> {
        let do_refresh = params.refresh_token;

        self.refresh_token_if_requested(do_refresh).await;

        let config = self.load_latest_config().await;
        let provider =
            create_model_provider(config.model_provider, Some(self.auth_manager.clone()));
        let account_state = match provider.account_state() {
            Ok(account_state) => account_state,
            Err(err) => return Err(invalid_request(err.to_string())),
        };
        let account = account_state.account.map(Account::from);

        Ok(GetAccountResponse {
            account,
            requires_openai_auth: account_state.requires_openai_auth,
        })
    }

    fn current_auth_dot_json(&self) -> std::result::Result<Option<AuthDotJson>, JSONRPCErrorError> {
        let keyring_backend_kind = self.config.auth_keyring_backend_kind();
        load_auth_dot_json(
            &self.config.codex_home,
            AuthCredentialsStoreMode::Ephemeral,
            keyring_backend_kind,
        )
        .and_then(|auth| {
            if auth.is_some() {
                Ok(auth)
            } else {
                load_auth_dot_json(
                    &self.config.codex_home,
                    self.config.cli_auth_credentials_store_mode,
                    keyring_backend_kind,
                )
            }
        })
        .map_err(|err| internal_error(format!("failed to load current auth state: {err}")))
    }

    fn clear_inactive_auth_storage(
        &self,
        active_store_mode: AuthCredentialsStoreMode,
    ) -> std::result::Result<(), JSONRPCErrorError> {
        let keyring_backend_kind = self.config.auth_keyring_backend_kind();
        if active_store_mode != AuthCredentialsStoreMode::Ephemeral {
            logout(
                &self.config.codex_home,
                AuthCredentialsStoreMode::Ephemeral,
                keyring_backend_kind,
            )
            .map_err(|err| {
                internal_error(format!("failed to clear inactive external auth: {err}"))
            })?;
        }

        if self.config.cli_auth_credentials_store_mode != AuthCredentialsStoreMode::Ephemeral
            && active_store_mode != self.config.cli_auth_credentials_store_mode
        {
            logout(
                &self.config.codex_home,
                self.config.cli_auth_credentials_store_mode,
                keyring_backend_kind,
            )
            .map_err(|err| {
                internal_error(format!("failed to clear inactive stored auth: {err}"))
            })?;
        }

        Ok(())
    }

    async fn list_auth_profiles_response(
        &self,
    ) -> Result<AuthProfileListResponse, JSONRPCErrorError> {
        let current_auth = self.current_auth_dot_json()?;
        let profiles = self
            .load_auth_profiles_with_rate_limits(current_auth.as_ref())
            .await?;
        Ok(AuthProfileListResponse { profiles })
    }

    async fn save_auth_profile_response(
        &self,
        params: AuthProfileSaveParams,
    ) -> Result<AuthProfileSaveResponse, JSONRPCErrorError> {
        let current_auth = self.current_auth_dot_json()?.ok_or_else(|| {
            invalid_request(
                "no stored auth is available to save as a profile; env-only API keys cannot be exported",
            )
        })?;

        let profile = save_auth_profile(
            &self.config.codex_home,
            &params.name,
            &current_auth,
            params.overwrite,
            Some(&current_auth),
        )
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::AlreadyExists => {
                invalid_request(format!("failed to save auth profile: {err}"))
            }
            std::io::ErrorKind::InvalidInput => {
                invalid_params(format!("failed to save auth profile: {err}"))
            }
            _ => internal_error(format!("failed to save auth profile: {err}")),
        })?;

        self.auth_profile_rate_limit_cache
            .lock()
            .await
            .remove(&params.name);
        Ok(AuthProfileSaveResponse { profile })
    }

    async fn activate_next_auth_profile_response(
        &self,
    ) -> Result<AuthProfileActivateNextResponse, JSONRPCErrorError> {
        let current_auth = self.current_auth_dot_json()?;
        let profiles = self
            .load_auth_profiles_with_rate_limits(current_auth.as_ref())
            .await?;
        let Some(profile) = select_next_auth_profile(&profiles) else {
            return Err(invalid_request(
                "no alternative auth profile with available ChatGPT capacity",
            ));
        };

        let response = self.activate_auth_profile_by_name(&profile.name).await?;
        Ok(AuthProfileActivateNextResponse {
            profile: response.profile,
            current_account: response.current_account,
        })
    }

    async fn activate_auth_profile_by_name(
        &self,
        name: &str,
    ) -> Result<AuthProfileActivateResponse, JSONRPCErrorError> {
        let profile_auth =
            load_auth_profile(&self.config.codex_home, name).map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => {
                    invalid_request(format!("failed to load auth profile: {err}"))
                }
                std::io::ErrorKind::InvalidInput => {
                    invalid_params(format!("failed to load auth profile: {err}"))
                }
                _ => internal_error(format!("failed to load auth profile: {err}")),
            })?;

        let target_store_mode = if matches!(
            profile_auth.auth_mode,
            Some(CoreAuthMode::ChatgptAuthTokens)
        ) {
            AuthCredentialsStoreMode::Ephemeral
        } else {
            self.config.cli_auth_credentials_store_mode
        };

        save_auth(
            &self.config.codex_home,
            &profile_auth,
            target_store_mode,
            self.config.auth_keyring_backend_kind(),
        )
        .map_err(|err| internal_error(format!("failed to activate auth profile: {err}")))?;
        self.clear_inactive_auth_storage(target_store_mode)?;

        self.auth_manager.reload().await;
        self.config_manager.replace_cloud_config_bundle_loader(
            self.auth_manager.clone(),
            self.config.chatgpt_base_url.clone(),
            self.config.http_client_factory(),
        );
        self.config_manager
            .sync_default_client_residency_requirement()
            .await;
        Self::maybe_refresh_plugin_caches_for_current_config(
            &self.config_manager,
            &self.thread_manager,
            self.auth_manager.auth_cached(),
        )
        .await;

        let current_auth = self.current_auth_dot_json()?;
        let profiles = self
            .load_auth_profiles_with_rate_limits(current_auth.as_ref())
            .await?;
        let Some(profile) = profiles.into_iter().find(|profile| profile.name == name) else {
            return Err(internal_error(
                "activated auth profile disappeared unexpectedly",
            ));
        };

        Ok(AuthProfileActivateResponse {
            current_account: profile.account.clone(),
            profile,
        })
    }

    async fn load_auth_profiles_with_rate_limits(
        &self,
        current_auth: Option<&AuthDotJson>,
    ) -> Result<Vec<AuthProfileSummary>, JSONRPCErrorError> {
        let mut profiles = list_auth_profiles(&self.config.codex_home, current_auth)
            .map_err(|err| internal_error(format!("failed to list auth profiles: {err}")))?;

        for profile in &mut profiles {
            let Ok(auth) = load_auth_profile(&self.config.codex_home, &profile.name) else {
                continue;
            };
            profile.rate_limits = self
                .cached_rate_limits_for_profile(&profile.name, &auth)
                .await
                .map(Into::into);
        }

        Ok(profiles)
    }

    async fn delete_auth_profile_response(
        &self,
        params: AuthProfileDeleteParams,
    ) -> Result<AuthProfileDeleteResponse, JSONRPCErrorError> {
        let deleted =
            delete_auth_profile(&self.config.codex_home, &params.name).map_err(|err| {
                if err.kind() == std::io::ErrorKind::InvalidInput {
                    invalid_params(format!("failed to delete auth profile: {err}"))
                } else {
                    internal_error(format!("failed to delete auth profile: {err}"))
                }
            })?;

        if deleted {
            self.auth_profile_rate_limit_cache
                .lock()
                .await
                .remove(&params.name);
        }
        Ok(AuthProfileDeleteResponse { deleted })
    }

    async fn cached_rate_limits_for_profile(
        &self,
        profile_name: &str,
        auth: &AuthDotJson,
    ) -> Option<CoreRateLimitSnapshot> {
        if let Some(cached) = self.cached_profile_rate_limits(profile_name).await {
            return Some(cached);
        }

        let snapshot = self.try_fetch_rate_limits_for_profile_auth(auth).await?;
        self.auth_profile_rate_limit_cache.lock().await.insert(
            profile_name.to_string(),
            CachedAuthProfileRateLimits {
                fetched_at: Instant::now(),
                snapshot: snapshot.clone(),
            },
        );
        Some(snapshot)
    }

    async fn cached_profile_rate_limits(
        &self,
        profile_name: &str,
    ) -> Option<CoreRateLimitSnapshot> {
        let cache = self.auth_profile_rate_limit_cache.lock().await;
        let cached = cache.get(profile_name)?;
        if cached.fetched_at.elapsed() > AUTH_PROFILE_RATE_LIMIT_CACHE_TTL {
            return None;
        }
        Some(cached.snapshot.clone())
    }

    async fn try_fetch_rate_limits_for_profile_auth(
        &self,
        auth: &AuthDotJson,
    ) -> Option<CoreRateLimitSnapshot> {
        let client = self.backend_client_for_profile_auth(auth).ok()??;
        let response = client.get_rate_limits_with_reset_credits().await.ok()?;
        response
            .rate_limits
            .iter()
            .find(|snapshot| snapshot.limit_id.as_deref() == Some("codex"))
            .cloned()
            .or_else(|| response.rate_limits.first().cloned())
    }

    fn backend_client_for_profile_auth(
        &self,
        auth: &AuthDotJson,
    ) -> std::io::Result<Option<BackendClient>> {
        if auth.openai_api_key.is_some() || matches!(auth.auth_mode, Some(CoreAuthMode::ApiKey)) {
            return Ok(None);
        }

        let Some(tokens) = auth.tokens.as_ref() else {
            return Err(IoError::other("chatgpt auth profile is missing tokens"));
        };
        if tokens.access_token.is_empty() {
            return Err(IoError::other(
                "chatgpt auth profile is missing access token",
            ));
        }

        let mut client = BackendClient::new(
            self.config.chatgpt_base_url.clone(),
            self.config.http_client_factory(),
        )
        .with_auth_provider(Arc::new(BearerAuthProvider::new(
            tokens.access_token.clone(),
        )));

        if let Some(account_id) = tokens
            .account_id
            .as_deref()
            .or(tokens.id_token.chatgpt_account_id.as_deref())
        {
            client = client.with_chatgpt_account_id(account_id);
        }

        if tokens.id_token.chatgpt_account_is_fedramp {
            client = client.with_fedramp_routing_header();
        }

        Ok(Some(client))
    }

    async fn get_account_rate_limits_response(
        &self,
        params: GetAccountRateLimitsParams,
    ) -> Result<GetAccountRateLimitsResponse, JSONRPCErrorError> {
        let Some(auth) = self.auth_manager.auth().await else {
            return Err(invalid_request(
                "codex account authentication required to read rate limits",
            ));
        };

        if !auth.uses_codex_backend() {
            return Err(invalid_request(
                "chatgpt authentication required to read rate limits",
            ));
        }

        let client = BackendClient::from_auth(
            self.config.chatgpt_base_url.clone(),
            &auth,
            self.config.http_client_factory(),
        );

        let usage_request = async {
            if params.supports_luna_reserve
                && auth.auth_mode() == codex_protocol::auth::AuthMode::Chatgpt
                && !auth.is_fedramp_account()
            {
                client.get_rate_limits_with_luna_reserve().await
            } else {
                client.get_rate_limits_with_reset_credits().await
            }
        };
        let (response, detailed_rate_limit_reset_credits) = tokio::join!(usage_request, async {
            if params.exclude_reset_credit_details {
                None
            } else {
                Self::detailed_rate_limit_reset_credits(&client).await
            }
        },);
        let response = response
            .map_err(|err| internal_error(format!("failed to fetch codex rate limits: {err}")))?;
        if response.rate_limits.is_empty() {
            return Err(internal_error(
                "failed to fetch codex rate limits: no snapshots returned",
            ));
        }

        let rate_limits_by_limit_id: HashMap<_, _> = response
            .rate_limits
            .iter()
            .cloned()
            .map(|snapshot| {
                let limit_id = snapshot
                    .limit_id
                    .clone()
                    .unwrap_or_else(|| "codex".to_string());
                (limit_id, snapshot)
            })
            .collect();
        let rate_limits = response
            .rate_limits
            .iter()
            .find(|snapshot| snapshot.limit_id.as_deref() == Some("codex"))
            .cloned()
            .unwrap_or_else(|| response.rate_limits[0].clone());

        let rate_limit_reset_credits = detailed_rate_limit_reset_credits.or_else(|| {
            response
                .rate_limit_reset_credits
                .map(|summary| RateLimitResetCreditsSummary {
                    available_count: summary.available_count,
                    credits: None,
                })
        });

        // Match desktop's account readiness check before exposing account-bound CTA content.
        // Normal rate limits remain available when older backends omit identity or banner data.
        let matches_active_account = !auth.is_fedramp_account()
            && response.account_id.is_some()
            && response.account_id == auth.get_account_id()
            && response.user_id.is_some()
            && response.user_id == auth.get_chatgpt_user_id();
        let rate_limit_upsell = response
            .rate_limit_upsell
            .filter(|_| matches_active_account);

        Ok(GetAccountRateLimitsResponse {
            ordinary_usage_allowed: response
                .ordinary_usage_allowed
                .filter(|_| matches_active_account),
            rate_limits: rate_limits.into(),
            rate_limits_by_limit_id: Some(
                rate_limits_by_limit_id
                    .into_iter()
                    .map(|(limit_id, snapshot)| (limit_id, snapshot.into()))
                    .collect(),
            ),
            rate_limit_reset_credits,
            account_id: response.account_id,
            rate_limit_upsell,
        })
    }

    async fn get_account_token_usage_response(
        &self,
        params: Option<GetAccountTokenUsageParams>,
    ) -> Result<GetAccountTokenUsageResponse, JSONRPCErrorError> {
        let thread_id = params
            .and_then(|params| params.thread_id)
            .map(|thread_id| {
                ThreadId::from_string(&thread_id)
                    .map_err(|err| invalid_request(format!("invalid thread id: {err}")))
            })
            .transpose()?;

        let Some(auth) = self.auth_manager.auth().await else {
            return Err(invalid_request(
                "codex account authentication required to read token usage",
            ));
        };

        if !auth.uses_codex_backend() {
            return Err(invalid_request(
                "chatgpt authentication required to read token usage",
            ));
        }

        let client = BackendClient::from_auth(
            self.config.chatgpt_base_url.clone(),
            &auth,
            self.config.http_client_factory(),
        );
        if let Some(thread_id) = thread_id {
            let thread_id = thread_id.to_string();
            let usage = tokio::time::timeout(
                THREAD_USAGE_FETCH_TIMEOUT,
                client.get_thread_usage(&thread_id),
            )
            .await
            .map_err(|_| internal_error("thread usage fetch timed out"))?;
            let thread_usage = match usage {
                Ok(usage) => Some(codex_app_server_protocol::ThreadUsage {
                    thread_id: usage.thread_id,
                    estimated_usage_credits_micros: usage.estimated_usage_credits_micros,
                    estimated_usage_usd_micros: usage.estimated_usage_usd_micros,
                    groups: usage
                        .groups
                        .into_iter()
                        .map(
                            |group| codex_app_server_protocol::ThreadUsageBreakdownGroup {
                                model: group.model,
                                reasoning_effort: group.reasoning_effort,
                                speed: group.speed,
                                estimated_usage_credits_micros: group
                                    .estimated_usage_credits_micros,
                                net_new_input_tokens: group.net_new_input_tokens,
                                cached_input_tokens: group.cached_input_tokens,
                                input_tokens: group.input_tokens,
                                output_tokens: group.output_tokens,
                                total_tokens: group.total_tokens,
                            },
                        )
                        .collect(),
                }),
                Err(err)
                    if matches!(err.status().map(|status| status.as_u16()), Some(403 | 404)) =>
                {
                    None
                }
                Err(err) => {
                    return Err(internal_error(format!(
                        "failed to fetch thread usage: {err}"
                    )));
                }
            };
            return Ok(GetAccountTokenUsageResponse {
                summary: AccountTokenUsageSummary {
                    lifetime_tokens: None,
                    peak_daily_tokens: None,
                    longest_running_turn_sec: None,
                    current_streak_days: None,
                    longest_streak_days: None,
                },
                daily_usage_buckets: None,
                thread_usage,
            });
        }
        let profile = tokio::time::timeout(
            ACCOUNT_TOKEN_USAGE_FETCH_TIMEOUT,
            client.get_token_usage_profile(),
        )
        .await
        .map_err(|_| internal_error("token usage profile fetch timed out"))?
        .map_err(|err| internal_error(format!("failed to fetch token usage profile: {err}")))?;
        Ok(Self::account_token_usage_response(profile))
    }

    async fn get_workspace_messages_response(
        &self,
    ) -> Result<GetWorkspaceMessagesResponse, JSONRPCErrorError> {
        let Some(auth) = self.auth_manager.auth().await else {
            return Err(invalid_request(
                "codex account authentication required to read workspace messages",
            ));
        };

        if !auth.uses_codex_backend() {
            return Err(invalid_request(
                "chatgpt authentication required to read workspace messages",
            ));
        }

        let client = BackendClient::from_auth(
            self.config.chatgpt_base_url.clone(),
            &auth,
            self.config.http_client_factory(),
        );
        let messages = tokio::time::timeout(
            ACCOUNT_WORKSPACE_MESSAGES_FETCH_TIMEOUT,
            client.list_workspace_messages(),
        )
        .await
        .map_err(|_| internal_error("workspace messages fetch timed out"))?;

        match messages {
            Ok(messages) => {
                Self::workspace_messages_response(messages, /*feature_enabled*/ true)
            }
            Err(err) if workspace_messages_feature_disabled(&err) => {
                Self::workspace_messages_response(
                    BackendWorkspaceMessagesResponse {
                        messages: Vec::new(),
                    },
                    /*feature_enabled*/ false,
                )
            }
            Err(err) => Err(internal_error(format!(
                "failed to fetch workspace messages: {err}"
            ))),
        }
    }

    fn account_token_usage_response(profile: TokenUsageProfile) -> GetAccountTokenUsageResponse {
        let stats = profile.stats;
        GetAccountTokenUsageResponse {
            summary: AccountTokenUsageSummary {
                lifetime_tokens: stats.lifetime_tokens,
                peak_daily_tokens: stats.peak_daily_tokens,
                longest_running_turn_sec: stats.longest_running_turn_sec,
                current_streak_days: stats.current_streak_days,
                longest_streak_days: stats.longest_streak_days,
            },
            daily_usage_buckets: stats.daily_usage_buckets.map(|buckets| {
                buckets
                    .into_iter()
                    .map(|bucket| AccountTokenUsageDailyBucket {
                        start_date: bucket.start_date,
                        tokens: bucket.tokens,
                    })
                    .collect()
            }),
            thread_usage: None,
        }
    }

    fn workspace_messages_response(
        messages: BackendWorkspaceMessagesResponse,
        feature_enabled: bool,
    ) -> Result<GetWorkspaceMessagesResponse, JSONRPCErrorError> {
        Ok(GetWorkspaceMessagesResponse {
            feature_enabled,
            messages: messages
                .messages
                .into_iter()
                .map(workspace_message_from_backend)
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    async fn send_add_credits_nudge_email_response(
        &self,
        params: SendAddCreditsNudgeEmailParams,
    ) -> Result<SendAddCreditsNudgeEmailResponse, JSONRPCErrorError> {
        self.send_add_credits_nudge_email_inner(params)
            .await
            .map(|status| SendAddCreditsNudgeEmailResponse { status })
    }

    async fn send_add_credits_nudge_email_inner(
        &self,
        params: SendAddCreditsNudgeEmailParams,
    ) -> Result<AddCreditsNudgeEmailStatus, JSONRPCErrorError> {
        let Some(auth) = self.auth_manager.auth().await else {
            return Err(invalid_request(
                "codex account authentication required to notify workspace owner",
            ));
        };

        if !auth.uses_codex_backend() {
            return Err(invalid_request(
                "chatgpt authentication required to notify workspace owner",
            ));
        }

        let client = BackendClient::from_auth(
            self.config.chatgpt_base_url.clone(),
            &auth,
            self.config.http_client_factory(),
        );

        match client
            .send_add_credits_nudge_email(Self::backend_credit_type(params.credit_type))
            .await
        {
            Ok(()) => Ok(AddCreditsNudgeEmailStatus::Sent),
            Err(err) if err.status().is_some_and(|status| status.as_u16() == 429) => {
                Ok(AddCreditsNudgeEmailStatus::CooldownActive)
            }
            Err(err) => Err(internal_error(format!(
                "failed to notify workspace owner: {err}"
            ))),
        }
    }

    fn backend_credit_type(value: AddCreditsNudgeCreditType) -> BackendAddCreditsNudgeCreditType {
        match value {
            AddCreditsNudgeCreditType::Credits => BackendAddCreditsNudgeCreditType::Credits,
            AddCreditsNudgeCreditType::UsageLimit => BackendAddCreditsNudgeCreditType::UsageLimit,
        }
    }

    fn account_from_codex_auth(auth: &CodexAuth) -> Option<Account> {
        if auth.is_api_key_auth() {
            return Some(Account::ApiKey {});
        }
        if matches!(
            auth,
            CodexAuth::BedrockApiKey(_) | CodexAuth::BedrockAccessKeys(_)
        ) {
            return Some(Account::AmazonBedrock {
                uses_codex_managed_credentials: true,
            });
        }
        if auth.is_chatgpt_auth() {
            return match (auth.get_account_email(), auth.account_plan_type()) {
                (Some(email), Some(plan_type)) => Some(Account::Chatgpt {
                    email: Some(email),
                    plan_type,
                }),
                _ => None,
            };
        }
        None
    }
}

fn workspace_message_from_backend(
    message: BackendWorkspaceMessage,
) -> Result<WorkspaceMessage, JSONRPCErrorError> {
    Ok(WorkspaceMessage {
        message_id: message.message_id,
        message_type: workspace_message_type_from_backend(message.message_type),
        message_body: message.message_body,
        created_at: workspace_message_timestamp_from_backend(message.created_at)?,
        archived_at: workspace_message_timestamp_from_backend(message.archived_at)?,
    })
}

fn workspace_message_timestamp_from_backend(
    timestamp: Option<String>,
) -> Result<Option<i64>, JSONRPCErrorError> {
    timestamp
        .map(|timestamp| {
            DateTime::parse_from_rfc3339(&timestamp)
                .map(|timestamp| timestamp.timestamp())
                .map_err(|err| {
                    internal_error(format!(
                        "failed to parse workspace message timestamp `{timestamp}`: {err}"
                    ))
                })
        })
        .transpose()
}

fn workspace_message_type_from_backend(
    message_type: BackendWorkspaceMessageType,
) -> WorkspaceMessageType {
    match message_type {
        BackendWorkspaceMessageType::Headline => WorkspaceMessageType::Headline,
        BackendWorkspaceMessageType::Announcement => WorkspaceMessageType::Announcement,
        BackendWorkspaceMessageType::Unknown => WorkspaceMessageType::Unknown,
    }
}

fn workspace_messages_feature_disabled(err: &BackendRequestError) -> bool {
    err.status().is_some_and(|status| status.as_u16() == 404)
}

#[cfg(test)]
mod tests {
    use super::*;
    use codex_backend_client::TokenUsageProfileDailyBucket;
    use codex_backend_client::TokenUsageProfileStats;
    use http::StatusCode;
    use pretty_assertions::assert_eq;

    #[test]
    fn account_token_usage_response_maps_profile_stats_and_daily_buckets() {
        let response = AccountRequestProcessor::account_token_usage_response(TokenUsageProfile {
            stats: TokenUsageProfileStats {
                lifetime_tokens: Some(123),
                peak_daily_tokens: Some(45),
                longest_running_turn_sec: Some(67),
                current_streak_days: Some(8),
                longest_streak_days: Some(9),
                daily_usage_buckets: Some(vec![TokenUsageProfileDailyBucket {
                    start_date: "2026-05-29".to_string(),
                    tokens: 10,
                }]),
            },
        });

        assert_eq!(
            response,
            GetAccountTokenUsageResponse {
                summary: AccountTokenUsageSummary {
                    lifetime_tokens: Some(123),
                    peak_daily_tokens: Some(45),
                    longest_running_turn_sec: Some(67),
                    current_streak_days: Some(8),
                    longest_streak_days: Some(9),
                },
                daily_usage_buckets: Some(vec![AccountTokenUsageDailyBucket {
                    start_date: "2026-05-29".to_string(),
                    tokens: 10,
                }]),
                thread_usage: None,
            }
        );
    }

    #[test]
    fn workspace_messages_response_maps_backend_messages() {
        let response = AccountRequestProcessor::workspace_messages_response(
            BackendWorkspaceMessagesResponse {
                messages: vec![BackendWorkspaceMessage {
                    message_id: "headline-id".to_string(),
                    message_type: BackendWorkspaceMessageType::Headline,
                    message_body: "Headline body".to_string(),
                    created_at: Some("2026-06-14T00:00:00Z".to_string()),
                    archived_at: Some("2026-06-15T00:00:00Z".to_string()),
                }],
            },
            /*feature_enabled*/ true,
        )
        .expect("workspace message timestamps should parse");

        assert_eq!(
            response,
            GetWorkspaceMessagesResponse {
                feature_enabled: true,
                messages: vec![WorkspaceMessage {
                    message_id: "headline-id".to_string(),
                    message_type: WorkspaceMessageType::Headline,
                    message_body: "Headline body".to_string(),
                    created_at: Some(1_781_395_200),
                    archived_at: Some(1_781_481_600),
                }],
            }
        );
    }

    #[test]
    fn workspace_messages_feature_disabled_only_for_not_found() {
        let cases = [
            (StatusCode::NOT_FOUND, true),
            (StatusCode::UNAUTHORIZED, false),
            (StatusCode::FORBIDDEN, false),
        ];

        for (status, expected) in cases {
            let err = BackendRequestError::UnexpectedStatus {
                method: "GET".to_string(),
                url: "https://example.test/api/codex/workspace-messages".to_string(),
                status,
                content_type: "application/json".to_string(),
                body: "{}".to_string(),
            };
            assert_eq!(workspace_messages_feature_disabled(&err), expected);
        }
    }
}
