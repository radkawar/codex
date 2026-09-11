//! A priming pass sends one bounded request with no tools or project context.

use codex_api::ResponseEvent;
use codex_api::ResponsesClient;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_http_client::ClientRouteClass;
use codex_login::AuthManager;
use codex_model_provider::auth_provider_from_auth;
use codex_model_provider::create_model_provider;
use codex_models_manager::manager::RefreshStrategy;
use codex_protocol::protocol::RateLimitSnapshot;
use futures::StreamExt;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub(super) const REQUEST_TIMEOUT: Duration = Duration::from_secs(180);

async fn cancellable<T>(
    cancel: &CancellationToken,
    request: impl Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(io::Error::new(io::ErrorKind::Interrupted, "account priming canceled")),
        result = tokio::time::timeout(REQUEST_TIMEOUT, request) => {
            result.map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "account priming request timed out"))?
        }
    }
}

pub(super) async fn prime(
    config: &Config,
    threads: &ThreadManager,
    manager: &Arc<AuthManager>,
    cancel: &CancellationToken,
) -> io::Result<()> {
    let auth = manager
        .auth()
        .await
        .ok_or_else(|| io::Error::other("saved account has no auth"))?;
    // Priming belongs to the saved ChatGPT account, even while an unrelated provider is selected.
    let openai = config
        .model_providers
        .get("openai")
        .ok_or_else(|| io::Error::other("OpenAI provider is unavailable for account priming"))?;
    let provider = create_model_provider(openai.clone(), Some(Arc::clone(manager)));
    let (models, requested_model) = if config.model_provider_id == "openai" {
        (threads.get_models_manager(), config.model.clone())
    } else {
        (
            provider.models_manager_without_cache(/*config_model_catalog*/ None),
            None,
        )
    };
    let model = models
        .get_default_model(
            &requested_model,
            /*allow_provider_model_fallback*/ false,
            RefreshStrategy::Offline,
            config.http_client_factory(),
        )
        .await;
    let provider = provider.api_provider().await.map_err(io::Error::other)?;
    let client = codex_login::default_client::create_client_for_route(
        &config.http_client_factory(),
        &provider.url_for_path("responses"),
        ClientRouteClass::Api,
    )
    .map_err(io::Error::other)?;
    let responses = ResponsesClient::new(
        codex_api::ReqwestTransport::from_http_client(client),
        provider,
        auth_provider_from_auth(&auth),
    );
    cancellable(cancel, async {
        let mut stream = responses
            .stream(
                serde_json::json!({
                    "model": model,
                    "instructions": "Reply briefly to the greeting.",
                    "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}],
                    "tools": [], "tool_choice": "none", "parallel_tool_calls": false,
                    "store": false, "stream": true,
                }),
                Default::default(),
                codex_api::Compression::default(),
                /*turn_state*/ None,
            )
            .await
            .map_err(io::Error::other)?;
        while let Some(event) = stream.next().await {
            if matches!(
                event.map_err(io::Error::other)?,
                ResponseEvent::Completed { .. }
            ) {
                return Ok(());
            }
        }
        Err(io::Error::other("priming response ended before completion"))
    })
    .await
}

pub(super) fn rate_limits_are_active(snapshot: &RateLimitSnapshot) -> bool {
    snapshot.primary.is_some() && snapshot.secondary.is_some()
}
