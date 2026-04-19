use std::io::Error as IoError;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use codex_app_server_protocol::Account;
use codex_app_server_protocol::AccountPrimingProfileOutcome;
use codex_app_server_protocol::AccountPrimingProfileResult;
use codex_app_server_protocol::AccountPrimingRunSummary;
use codex_app_server_protocol::AccountPrimingStatus;
use codex_app_server_protocol::AuthMode;
use codex_app_server_protocol::RateLimitSnapshot as ApiRateLimitSnapshot;
use codex_core::NewThread;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthManager;
use codex_login::load_auth_dot_json;
use codex_login::save_auth;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::user_input::UserInput;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::auth_profiles::list_auth_profiles;
use crate::auth_profiles::load_auth_profile;
use codex_backend_client::Client as BackendClient;

pub(crate) const DEFAULT_ACCOUNT_PRIMING_INTERVAL_SECONDS: u32 = 300;
const ACCOUNT_PRIMING_PROMPT: &str = "hi";
const ACCOUNT_PRIMING_TURN_TIMEOUT: Duration = Duration::from_secs(180);

pub(crate) struct AccountPrimingController {
    shared: Arc<AccountPrimingShared>,
}

struct AccountPrimingShared {
    config: Arc<Config>,
    thread_manager: Arc<ThreadManager>,
    state: Mutex<AccountPrimingState>,
}

struct AccountPrimingState {
    worker: Option<AccountPrimingWorker>,
    last_run: Option<AccountPrimingRunSummary>,
}

struct AccountPrimingWorker {
    cancel: CancellationToken,
    join: JoinHandle<()>,
    interval_seconds: u32,
    started_at: i64,
    current_run_started_at: Option<i64>,
    current_profile_name: Option<String>,
}

impl AccountPrimingController {
    pub(crate) fn new(config: Arc<Config>, thread_manager: Arc<ThreadManager>) -> Self {
        Self {
            shared: Arc::new(AccountPrimingShared {
                config,
                thread_manager,
                state: Mutex::new(AccountPrimingState {
                    worker: None,
                    last_run: None,
                }),
            }),
        }
    }

    pub(crate) async fn read_status(&self) -> AccountPrimingStatus {
        self.shared.read_status().await
    }

    pub(crate) async fn start(
        &self,
        interval_seconds: u32,
    ) -> Result<AccountPrimingStatus, IoError> {
        let started_at = now_ts();
        let cancel = CancellationToken::new();
        let shared = Arc::clone(&self.shared);
        let cancel_for_worker = cancel.clone();
        let join = tokio::spawn(async move {
            shared
                .run_background_worker(interval_seconds, started_at, cancel_for_worker)
                .await;
        });

        let mut state = self.shared.state.lock().await;
        if state.worker.is_some() {
            join.abort();
            return Err(IoError::other("account priming is already running"));
        }

        state.worker = Some(AccountPrimingWorker {
            cancel,
            join,
            interval_seconds,
            started_at,
            current_run_started_at: None,
            current_profile_name: None,
        });
        Ok(AccountPrimingStatus {
            running: true,
            interval_seconds: Some(interval_seconds),
            started_at: Some(started_at),
            current_run_started_at: None,
            current_profile_name: None,
            last_run: state.last_run.clone(),
        })
    }

    pub(crate) async fn stop(&self) -> AccountPrimingStatus {
        self.shared.stop().await
    }

    pub(crate) async fn run_once(&self) -> Result<AccountPrimingRunSummary, IoError> {
        self.shared.run_once().await
    }

    pub(crate) async fn shutdown(&self) {
        self.shared.shutdown().await;
    }
}

impl AccountPrimingShared {
    async fn read_status(&self) -> AccountPrimingStatus {
        let state = self.state.lock().await;
        let worker = state.worker.as_ref();
        AccountPrimingStatus {
            running: worker.is_some(),
            interval_seconds: worker.map(|worker| worker.interval_seconds),
            started_at: worker.map(|worker| worker.started_at),
            current_run_started_at: worker.and_then(|worker| worker.current_run_started_at),
            current_profile_name: worker.and_then(|worker| worker.current_profile_name.clone()),
            last_run: state.last_run.clone(),
        }
    }

    async fn stop(&self) -> AccountPrimingStatus {
        let worker = {
            let mut state = self.state.lock().await;
            state.worker.take()
        };

        if let Some(worker) = worker {
            worker.cancel.cancel();
            let _ = worker.join.await;
        }

        self.read_status().await
    }

    async fn run_once(&self) -> Result<AccountPrimingRunSummary, IoError> {
        if self.state.lock().await.worker.is_some() {
            return Err(IoError::other(
                "account priming is already running; stop it before running a manual pass",
            ));
        }

        let summary = self.run_pass(None, None).await;
        self.state.lock().await.last_run = Some(summary.clone());
        Ok(summary)
    }

    async fn shutdown(&self) {
        let _status = self.stop().await;
    }

    async fn run_background_worker(
        self: Arc<Self>,
        interval_seconds: u32,
        worker_started_at: i64,
        cancel: CancellationToken,
    ) {
        loop {
            let summary = self.run_pass(Some(worker_started_at), Some(&cancel)).await;
            self.record_background_run(worker_started_at, summary).await;

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(u64::from(interval_seconds))) => {}
            }
        }

        let mut state = self.state.lock().await;
        if let Some(worker) = state.worker.as_ref()
            && worker.started_at == worker_started_at
        {
            state.worker = None;
        }
    }

    async fn record_background_run(
        &self,
        worker_started_at: i64,
        summary: AccountPrimingRunSummary,
    ) {
        let mut state = self.state.lock().await;
        state.last_run = Some(summary);
        if let Some(worker) = state.worker.as_mut()
            && worker.started_at == worker_started_at
        {
            worker.current_run_started_at = None;
            worker.current_profile_name = None;
        }
    }

    async fn run_pass(
        &self,
        worker_started_at: Option<i64>,
        cancel: Option<&CancellationToken>,
    ) -> AccountPrimingRunSummary {
        let started_at = now_ts();
        if let Some(worker_started_at) = worker_started_at {
            self.set_worker_run_state(worker_started_at, Some(started_at), None)
                .await;
        }

        let mut cancelled = false;
        let mut primed_count = 0;
        let mut already_active_count = 0;
        let mut unsupported_count = 0;
        let mut failed_count = 0;
        let mut results = Vec::new();

        match list_auth_profiles(&self.config.codex_home, None) {
            Ok(profiles) => {
                for profile in profiles {
                    if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
                        cancelled = true;
                        break;
                    }

                    if let Some(worker_started_at) = worker_started_at {
                        self.set_worker_run_state(
                            worker_started_at,
                            Some(started_at),
                            Some(profile.name.clone()),
                        )
                        .await;
                    }

                    let result = self
                        .process_profile(&profile.name, profile.account.clone())
                        .await;
                    match result.outcome {
                        AccountPrimingProfileOutcome::Primed => primed_count += 1,
                        AccountPrimingProfileOutcome::AlreadyActive => already_active_count += 1,
                        AccountPrimingProfileOutcome::UnsupportedAuth => unsupported_count += 1,
                        AccountPrimingProfileOutcome::Failed => failed_count += 1,
                    }
                    results.push(result);
                }
            }
            Err(err) => {
                failed_count = 1;
                results.push(AccountPrimingProfileResult {
                    profile_name: "<profiles>".to_string(),
                    account: None,
                    outcome: AccountPrimingProfileOutcome::Failed,
                    before_rate_limits: None,
                    after_rate_limits: None,
                    error: Some(format!("failed to list auth profiles: {err}")),
                });
            }
        }

        AccountPrimingRunSummary {
            started_at,
            completed_at: now_ts(),
            cancelled,
            primed_count,
            already_active_count,
            unsupported_count,
            failed_count,
            results,
        }
    }

    async fn process_profile(
        &self,
        profile_name: &str,
        account: Option<Account>,
    ) -> AccountPrimingProfileResult {
        let auth = match load_auth_profile(&self.config.codex_home, profile_name) {
            Ok(auth) => auth,
            Err(err) => {
                return AccountPrimingProfileResult {
                    profile_name: profile_name.to_string(),
                    account,
                    outcome: AccountPrimingProfileOutcome::Failed,
                    before_rate_limits: None,
                    after_rate_limits: None,
                    error: Some(format!("failed to load auth profile: {err}")),
                };
            }
        };

        if matches!(resolved_auth_mode(&auth), ResolvedAuthMode::ApiKey) {
            return AccountPrimingProfileResult {
                profile_name: profile_name.to_string(),
                account,
                outcome: AccountPrimingProfileOutcome::UnsupportedAuth,
                before_rate_limits: None,
                after_rate_limits: None,
                error: Some("API key auth does not expose ChatGPT usage windows".to_string()),
            };
        }

        let before_rate_limits = match self.fetch_rate_limits_for_auth(&auth).await {
            Ok(snapshot) => snapshot,
            Err(err) => {
                return AccountPrimingProfileResult {
                    profile_name: profile_name.to_string(),
                    account,
                    outcome: AccountPrimingProfileOutcome::Failed,
                    before_rate_limits: None,
                    after_rate_limits: None,
                    error: Some(format!("failed to fetch rate limits before priming: {err}")),
                };
            }
        };

        if rate_limits_are_active(&before_rate_limits) {
            return AccountPrimingProfileResult {
                profile_name: profile_name.to_string(),
                account,
                outcome: AccountPrimingProfileOutcome::AlreadyActive,
                before_rate_limits: Some(ApiRateLimitSnapshot::from(before_rate_limits.clone())),
                after_rate_limits: Some(ApiRateLimitSnapshot::from(before_rate_limits)),
                error: None,
            };
        }

        match self.prime_profile(&auth).await {
            Ok(after_rate_limits) if rate_limits_are_active(&after_rate_limits) => {
                AccountPrimingProfileResult {
                    profile_name: profile_name.to_string(),
                    account,
                    outcome: AccountPrimingProfileOutcome::Primed,
                    before_rate_limits: Some(ApiRateLimitSnapshot::from(before_rate_limits)),
                    after_rate_limits: Some(ApiRateLimitSnapshot::from(after_rate_limits)),
                    error: None,
                }
            }
            Ok(after_rate_limits) => AccountPrimingProfileResult {
                profile_name: profile_name.to_string(),
                account,
                outcome: AccountPrimingProfileOutcome::Failed,
                before_rate_limits: Some(ApiRateLimitSnapshot::from(before_rate_limits)),
                after_rate_limits: Some(ApiRateLimitSnapshot::from(after_rate_limits)),
                error: Some("usage windows were still inactive after priming".to_string()),
            },
            Err(err) => AccountPrimingProfileResult {
                profile_name: profile_name.to_string(),
                account,
                outcome: AccountPrimingProfileOutcome::Failed,
                before_rate_limits: Some(ApiRateLimitSnapshot::from(before_rate_limits)),
                after_rate_limits: None,
                error: Some(format!("failed to prime account: {err}")),
            },
        }
    }

    async fn prime_profile(&self, auth: &AuthDotJson) -> Result<RateLimitSnapshot, IoError> {
        let temp_home = TempDir::new()?;
        let store_mode = auth_store_mode(auth);
        save_auth(temp_home.path(), auth, store_mode)?;
        let auth_manager = Arc::new(AuthManager::new(
            temp_home.path().to_path_buf(),
            /*enable_codex_api_key_env*/ false,
            store_mode,
        ));

        let mut config = (*self.config).clone();
        config.ephemeral = true;

        let NewThread {
            thread_id, thread, ..
        } = self
            .thread_manager
            .resume_thread_with_history(
                config,
                InitialHistory::New,
                auth_manager,
                /*persist_extended_history*/ false,
                /*parent_trace*/ None,
            )
            .await
            .map_err(|err| IoError::other(format!("failed to start priming thread: {err}")))?;

        let prime_result = self.run_prime_turn(thread.as_ref()).await;

        let shutdown_result = thread
            .shutdown_and_wait()
            .await
            .map_err(|err| IoError::other(format!("failed to shut down priming thread: {err}")));
        let _removed = self.thread_manager.remove_thread(&thread_id).await;

        prime_result?;
        shutdown_result?;

        let refreshed_auth = load_auth_dot_json(temp_home.path(), store_mode)?
            .ok_or_else(|| IoError::other("priming auth disappeared before rate-limit refresh"))?;
        self.fetch_rate_limits_for_auth(&refreshed_auth).await
    }

    async fn run_prime_turn(&self, thread: &codex_core::CodexThread) -> Result<(), IoError> {
        let turn_id = thread
            .submit(Op::UserInput {
                items: vec![UserInput::Text {
                    text: ACCOUNT_PRIMING_PROMPT.to_string(),
                    text_elements: Vec::new(),
                }],
                final_output_json_schema: None,
                responsesapi_client_metadata: None,
            })
            .await
            .map_err(|err| IoError::other(format!("failed to submit priming turn: {err}")))?;

        tokio::time::timeout(ACCOUNT_PRIMING_TURN_TIMEOUT, async {
            loop {
                let event = thread.next_event().await.map_err(|err| {
                    IoError::other(format!("failed to read priming event: {err}"))
                })?;

                match event.msg {
                    EventMsg::TurnComplete(event) if event.turn_id == turn_id => return Ok(()),
                    EventMsg::TurnAborted(event)
                        if event.turn_id.as_deref() == Some(turn_id.as_str()) =>
                    {
                        return Err(IoError::other(format!(
                            "priming turn aborted: {:?}",
                            event.reason
                        )));
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| IoError::other("timed out waiting for priming turn completion"))?
    }

    async fn fetch_rate_limits_for_auth(
        &self,
        auth: &AuthDotJson,
    ) -> Result<RateLimitSnapshot, IoError> {
        let client = backend_client_for_auth(self.config.chatgpt_base_url.as_str(), auth)?;
        let snapshots = client
            .get_rate_limits_many()
            .await
            .map_err(|err| IoError::other(format!("failed to fetch rate limits: {err}")))?;
        if snapshots.is_empty() {
            return Err(IoError::other(
                "failed to fetch rate limits: no snapshots returned",
            ));
        }

        Ok(snapshots
            .iter()
            .find(|snapshot| snapshot.limit_id.as_deref() == Some("codex"))
            .cloned()
            .unwrap_or_else(|| snapshots[0].clone()))
    }

    async fn set_worker_run_state(
        &self,
        worker_started_at: i64,
        current_run_started_at: Option<i64>,
        current_profile_name: Option<String>,
    ) {
        let mut state = self.state.lock().await;
        if let Some(worker) = state.worker.as_mut()
            && worker.started_at == worker_started_at
        {
            worker.current_run_started_at = current_run_started_at;
            worker.current_profile_name = current_profile_name;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolvedAuthMode {
    ApiKey,
    Chatgpt,
}

fn resolved_auth_mode(auth: &AuthDotJson) -> ResolvedAuthMode {
    if auth.openai_api_key.is_some() || matches!(auth.auth_mode, Some(AuthMode::ApiKey)) {
        ResolvedAuthMode::ApiKey
    } else {
        ResolvedAuthMode::Chatgpt
    }
}

fn auth_store_mode(auth: &AuthDotJson) -> AuthCredentialsStoreMode {
    if matches!(auth.auth_mode, Some(AuthMode::ChatgptAuthTokens)) {
        AuthCredentialsStoreMode::Ephemeral
    } else {
        AuthCredentialsStoreMode::File
    }
}

fn rate_limits_are_active(snapshot: &RateLimitSnapshot) -> bool {
    snapshot.primary.is_some() && snapshot.secondary.is_some()
}

fn backend_client_for_auth(
    chatgpt_base_url: &str,
    auth: &AuthDotJson,
) -> Result<BackendClient, IoError> {
    let mut client = BackendClient::new(chatgpt_base_url.to_string())
        .map_err(IoError::other)?
        .with_user_agent(codex_login::default_client::get_codex_user_agent());

    let Some(tokens) = auth.tokens.as_ref() else {
        return Err(IoError::other("chatgpt auth profile is missing tokens"));
    };
    if tokens.access_token.is_empty() {
        return Err(IoError::other(
            "chatgpt auth profile is missing access token",
        ));
    }

    client = client.with_bearer_token(tokens.access_token.clone());
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

    Ok(client)
}

fn now_ts() -> i64 {
    Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn snapshot(primary: bool, secondary: bool) -> RateLimitSnapshot {
        RateLimitSnapshot {
            limit_id: Some("codex".to_string()),
            limit_name: None,
            primary: primary.then_some(codex_protocol::protocol::RateLimitWindow {
                used_percent: 25.0,
                window_minutes: Some(300),
                resets_at: Some(1),
            }),
            secondary: secondary.then_some(codex_protocol::protocol::RateLimitWindow {
                used_percent: 10.0,
                window_minutes: Some(7 * 24 * 60),
                resets_at: Some(2),
            }),
            credits: None,
            plan_type: None,
        }
    }

    #[test]
    fn rate_limits_are_active_requires_both_windows() {
        assert_eq!(rate_limits_are_active(&snapshot(true, true)), true);
        assert_eq!(rate_limits_are_active(&snapshot(true, false)), false);
        assert_eq!(rate_limits_are_active(&snapshot(false, true)), false);
        assert_eq!(rate_limits_are_active(&snapshot(false, false)), false);
    }
}
