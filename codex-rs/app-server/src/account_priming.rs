//! Primes canonical saved accounts without exposing their credentials to active turns.

mod request;

use crate::account_session_auth::IsolatedAccountSessionAuth;
use crate::account_sessions::AccountSessionsStore;
use chrono::Utc;
use codex_app_server_protocol::AccountPrimingProfileOutcome;
use codex_app_server_protocol::AccountPrimingProfileResult;
use codex_app_server_protocol::AccountPrimingRunSummary;
use codex_app_server_protocol::AccountPrimingStatus;
use codex_app_server_protocol::AccountSession;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::auth::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) const DEFAULT_ACCOUNT_PRIMING_INTERVAL_SECONDS: u32 = 300;

#[derive(Clone)]
pub(crate) struct AccountPrimingController {
    shared: Arc<AccountPrimingShared>,
}

struct AccountPrimingShared {
    config: Arc<Config>,
    thread_manager: Arc<ThreadManager>,
    auth_manager: Arc<AuthManager>,
    state: Mutex<AccountPrimingState>,
}

#[derive(Default)]
struct AccountPrimingState {
    worker: Option<AccountPrimingWorker>,
    last_run: Option<AccountPrimingRunSummary>,
}

struct AccountPrimingWorker {
    id: Uuid,
    cancel: CancellationToken,
    completed: watch::Receiver<bool>,
    interval_seconds: Option<u32>,
    started_at: i64,
    current_run_started_at: Option<i64>,
    current_profile_name: Option<String>,
}

#[derive(Clone, Copy)]
enum RunMode {
    Once,
    Periodic(u32),
}

impl AccountPrimingState {
    fn status(&self) -> AccountPrimingStatus {
        let worker = self.worker.as_ref();
        AccountPrimingStatus {
            running: worker.is_some(),
            interval_seconds: worker.and_then(|worker| worker.interval_seconds),
            started_at: worker.map(|worker| worker.started_at),
            current_run_started_at: worker.and_then(|worker| worker.current_run_started_at),
            current_profile_name: worker.and_then(|worker| worker.current_profile_name.clone()),
            last_run: self.last_run.clone(),
        }
    }
}

impl AccountPrimingController {
    pub(crate) fn new(
        config: Arc<Config>,
        thread_manager: Arc<ThreadManager>,
        auth_manager: Arc<AuthManager>,
    ) -> Self {
        Self {
            shared: Arc::new(AccountPrimingShared {
                config,
                thread_manager,
                auth_manager,
                state: Mutex::new(AccountPrimingState::default()),
            }),
        }
    }

    pub(crate) async fn read_status(&self) -> AccountPrimingStatus {
        self.shared.state.lock().await.status()
    }

    pub(crate) async fn start(&self, interval_seconds: u32) -> io::Result<AccountPrimingStatus> {
        let (status, _completion) = self.launch(RunMode::Periodic(interval_seconds)).await?;
        Ok(status)
    }

    async fn launch(
        &self,
        mode: RunMode,
    ) -> io::Result<(
        AccountPrimingStatus,
        oneshot::Receiver<AccountPrimingRunSummary>,
    )> {
        // Reserve the worker before any work starts, including a manual pass.
        let mut state = self.shared.state.lock().await;
        if state.worker.is_some() {
            return Err(io::Error::other("account priming is already running"));
        }
        let id = Uuid::new_v4();
        let cancel = CancellationToken::new();
        let (completed, completion) = watch::channel(false);
        let (result_tx, result_rx) = oneshot::channel();
        state.worker = Some(AccountPrimingWorker {
            id,
            cancel: cancel.clone(),
            completed: completion,
            interval_seconds: match mode {
                RunMode::Once => None,
                RunMode::Periodic(interval) => Some(interval),
            },
            started_at: Utc::now().timestamp(),
            current_run_started_at: None,
            current_profile_name: None,
        });
        let shared = Arc::clone(&self.shared);
        tokio::spawn(async move {
            let summary = shared.run_worker(id, mode, cancel).await;
            let _ = result_tx.send(summary);
            let _ = completed.send(true);
        });
        Ok((state.status(), result_rx))
    }

    pub(crate) async fn stop(&self) -> AccountPrimingStatus {
        let worker = {
            let state = self.shared.state.lock().await;
            state.worker.as_ref().map(|worker| {
                worker.cancel.cancel();
                (worker.id, worker.completed.clone())
            })
        };
        if let Some((id, mut completion)) = worker {
            let _ = completion.wait_for(|completed| *completed).await;
            let mut state = self.shared.state.lock().await;
            if state.worker.as_ref().is_some_and(|worker| worker.id == id) {
                state.worker = None;
            }
        }
        self.read_status().await
    }

    pub(crate) async fn begin_run_once(
        &self,
    ) -> io::Result<oneshot::Receiver<AccountPrimingRunSummary>> {
        let (_status, completion) = self.launch(RunMode::Once).await?;
        Ok(completion)
    }

    pub(crate) async fn shutdown(&self) {
        self.stop().await;
    }
}

impl AccountPrimingShared {
    async fn run_worker(
        &self,
        id: Uuid,
        mode: RunMode,
        cancel: CancellationToken,
    ) -> AccountPrimingRunSummary {
        let summary = loop {
            let summary = self.run_pass(id, &cancel).await;
            {
                let mut state = self.state.lock().await;
                state.last_run = Some(summary.clone());
                if let Some(worker) = state.worker.as_mut()
                    && worker.id == id
                {
                    worker.current_run_started_at = None;
                    worker.current_profile_name = None;
                }
            }
            let interval = match mode {
                RunMode::Once => break summary,
                RunMode::Periodic(interval) => interval,
            };
            tokio::select! {
                _ = cancel.cancelled() => break summary,
                _ = tokio::time::sleep(Duration::from_secs(u64::from(interval))) => {}
            }
        };
        let mut state = self.state.lock().await;
        if state.worker.as_ref().is_some_and(|worker| worker.id == id) {
            state.worker = None;
        }
        summary
    }

    async fn run_pass(&self, id: Uuid, cancel: &CancellationToken) -> AccountPrimingRunSummary {
        let started_at = Utc::now().timestamp();
        let mut summary = AccountPrimingRunSummary {
            started_at,
            completed_at: started_at,
            cancelled: false,
            primed_count: 0,
            already_active_count: 0,
            unsupported_count: 0,
            failed_count: 0,
            results: Vec::new(),
        };
        match AccountSessionsStore::new(&self.config, &self.auth_manager)
            .list(/*refresh_workspace_metadata*/ false)
            .await
        {
            Ok(accounts) => {
                for account in accounts.sessions {
                    if cancel.is_cancelled() {
                        break;
                    }
                    {
                        let mut state = self.state.lock().await;
                        if let Some(worker) = state.worker.as_mut()
                            && worker.id == id
                        {
                            worker.current_run_started_at = Some(started_at);
                            worker.current_profile_name = Some(
                                account
                                    .email
                                    .clone()
                                    .unwrap_or_else(|| account.session_id.clone()),
                            );
                        }
                    }
                    let result = self.process_account(account, cancel).await;
                    if cancel.is_cancelled() {
                        break;
                    }
                    match result.outcome {
                        AccountPrimingProfileOutcome::Primed => summary.primed_count += 1,
                        AccountPrimingProfileOutcome::AlreadyActive => {
                            summary.already_active_count += 1
                        }
                        AccountPrimingProfileOutcome::UnsupportedAuth => {
                            summary.unsupported_count += 1
                        }
                        AccountPrimingProfileOutcome::Failed => summary.failed_count += 1,
                    }
                    summary.results.push(result);
                }
            }
            Err(error) => {
                summary.failed_count = 1;
                summary.results.push(AccountPrimingProfileResult {
                    profile_name: "<accounts>".to_string(),
                    account: None,
                    outcome: AccountPrimingProfileOutcome::Failed,
                    before_rate_limits: None,
                    after_rate_limits: None,
                    error: Some(format!("failed to list saved accounts: {error}")),
                });
            }
        }
        summary.cancelled = cancel.is_cancelled();
        summary.completed_at = Utc::now().timestamp();
        summary
    }

    async fn process_account(
        &self,
        account: AccountSession,
        cancel: &CancellationToken,
    ) -> AccountPrimingProfileResult {
        let mut result = AccountPrimingProfileResult {
            profile_name: account
                .email
                .clone()
                .unwrap_or_else(|| account.session_id.clone()),
            account: account.account,
            outcome: AccountPrimingProfileOutcome::Failed,
            before_rate_limits: None,
            after_rate_limits: None,
            error: None,
        };
        let store = AccountSessionsStore::new(&self.config, &self.auth_manager);
        let attempt = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "account priming canceled")),
            result = IsolatedAccountSessionAuth::new(&self.config, &store, &account.session_id) => result,
        };
        let isolated = match attempt {
            Ok(isolated) => isolated,
            Err(error) => {
                result.error = Some(format!("failed to load saved account: {error}"));
                return result;
            }
        };
        if !self
            .auth_manager
            .is_login_method_allowed(ForcedLoginMethod::Chatgpt)
            || !matches!(
                isolated.initial_auth().auth_mode,
                Some(AuthMode::Chatgpt) | None
            )
            || !matches!(
                isolated.auth_manager.auth_cached(),
                Some(CodexAuth::Chatgpt(_))
            )
        {
            result.outcome = AccountPrimingProfileOutcome::UnsupportedAuth;
            result.error = Some("Priming requires a saved ChatGPT login".to_string());
            return result;
        }
        let operation = async {
            let before = isolated
                .fetch_rate_limits(&self.config, request::REQUEST_TIMEOUT, cancel)
                .await?;
            result.before_rate_limits = Some(before.clone().into());
            if request::rate_limits_are_active(&before) {
                result.outcome = AccountPrimingProfileOutcome::AlreadyActive;
                result.after_rate_limits = Some(before.into());
                return Ok(());
            }
            request::prime(
                &self.config,
                &self.thread_manager,
                &isolated.auth_manager,
                cancel,
            )
            .await?;
            let after = isolated
                .fetch_rate_limits(&self.config, request::REQUEST_TIMEOUT, cancel)
                .await?;
            let active = request::rate_limits_are_active(&after);
            result.after_rate_limits = Some(after.into());
            if !active {
                return Err(io::Error::other(
                    "usage windows were still inactive after priming",
                ));
            }
            result.outcome = AccountPrimingProfileOutcome::Primed;
            Ok::<_, io::Error>(())
        }
        .await;
        // A refresh can rotate credentials even if the subsequent request fails or is canceled.
        let persisted = isolated.persist_if_changed(&store).await;
        if let Err(error) = persisted.map(|_| ()).and(operation) {
            result.outcome = AccountPrimingProfileOutcome::Failed;
            result.error = Some(format!("failed to prime account: {error}"));
        }
        result
    }
}
