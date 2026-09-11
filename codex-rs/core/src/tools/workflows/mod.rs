mod parser;
mod runtime;
mod spec;
mod store;

use crate::function_tool::FunctionCallError;
use crate::tools::code_mode::CodeModeExecuteHandler;
use crate::tools::code_mode::CodeModeNestedTool;
use crate::tools::code_mode::CodeModeNotificationOutput;
use crate::tools::code_mode::telemetry::CodeModeToolCallGuard;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::multi_agents_common::function_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use crate::tools::registry::ToolExposure;
use codex_protocol::models::ResponseInputItem;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use runtime::RuntimeInput;
use serde::Deserialize;
use serde_json::Value;
use spec::JOURNAL_TOOL_NAME;
use spec::LOAD_TOOL_NAME;
use spec::PUBLIC_TOOL_NAME;
use std::sync::Arc;
use store::JournalRecord;

pub(crate) struct WorkflowHandler {
    spec: ToolSpec,
    execute_handler: CodeModeExecuteHandler,
}

impl WorkflowHandler {
    pub(crate) fn new(nested_tool_specs: Vec<CodeModeNestedTool>) -> Self {
        let spec = spec::create_workflow_tool();
        Self {
            execute_handler: CodeModeExecuteHandler::new(
                spec.clone(),
                nested_tool_specs,
                CodeModeNotificationOutput::FunctionTool,
            ),
            spec,
        }
    }

    pub(crate) fn supports_nested_tool(tool_name: &ToolName) -> bool {
        (tool_name.namespace.is_none() || tool_name.namespace.as_deref() == Some("collaboration"))
            && matches!(
                tool_name.name.as_str(),
                "spawn_agent"
                    | "list_agents"
                    | "followup_task"
                    | JOURNAL_TOOL_NAME
                    | LOAD_TOOL_NAME
            )
    }

    #[tracing::instrument(
        name = "code_mode.handler.workflow",
        level = "info",
        skip_all,
        fields(
            conversation.id = %invocation.session.thread_id,
            turn_id = invocation.turn.sub_id.as_str(),
            call_id = invocation.call_id.as_str(),
            cell.id = tracing::field::Empty,
            outcome = "interrupted",
        )
    )]
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn ToolOutput>, FunctionCallError> {
        let handler_span = tracing::Span::current();
        let originating_item_id = invocation.originating_item_id().await;
        let ToolInvocation {
            session,
            turn,
            step_context,
            call_id,
            payload,
            ..
        } = invocation;
        let arguments = function_arguments(payload)?;
        let args: WorkflowArgs = serde_json::from_str(&arguments).map_err(|err| {
            FunctionCallError::RespondToModel(format!("invalid workflow arguments: {err}"))
        })?;
        let source_count = usize::from(args.script.is_some())
            + usize::from(args.script_path.is_some())
            + usize::from(args.name.is_some());
        if source_count != 1 {
            return Err(FunctionCallError::RespondToModel(
                "provide exactly one of script, scriptPath, or name".to_string(),
            ));
        }

        let source = match args.script {
            Some(source) => source,
            None => {
                store::load_source(
                    &turn,
                    session.thread_id,
                    args.name.as_deref(),
                    args.script_path.as_deref(),
                )
                .await
                .map_err(FunctionCallError::RespondToModel)?
                .0
            }
        };
        let parsed = store::parse_source(&source).map_err(FunctionCallError::RespondToModel)?;
        let meta = parsed.meta;
        let body = parsed.body.to_string();
        let run_id = args
            .resume_from_run_id
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let script_path =
            store::write_run_script(Arc::clone(&turn), session.thread_id, &run_id, source)
                .await
                .map_err(FunctionCallError::RespondToModel)?;
        let journal_path = store::journal_path(&turn, session.thread_id, &run_id)
            .map_err(FunctionCallError::RespondToModel)?;
        let journal = store::load_journal(journal_path)
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        let concurrency = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(2)
            .saturating_sub(2)
            .clamp(1, 16);
        let script_path_text = script_path.to_string_lossy().into_owned();
        let runtime_source = runtime::build(RuntimeInput {
            run_id: &run_id,
            script_path: &script_path_text,
            body: &body,
            meta: &meta,
            args: &args.args.unwrap_or(Value::Null),
            journal: &journal,
            concurrency,
        })
        .map_err(FunctionCallError::RespondToModel)?;

        let mut telemetry = CodeModeToolCallGuard::new(
            session.services.analytics_events_client.clone(),
            session.thread_id.to_string(),
            turn.sub_id.clone(),
            turn.turn_metadata_state.clone(),
            call_id.clone(),
            PUBLIC_TOOL_NAME,
            handler_span,
        );
        let result = self
            .execute_handler
            .execute(
                session,
                step_context,
                call_id,
                originating_item_id,
                runtime_source,
                &mut telemetry,
            )
            .await
            .map(boxed_tool_output);
        telemetry.finish(
            result
                .as_ref()
                .is_ok_and(codex_tools::ToolOutput::success_for_logging),
        );
        result
    }
}

impl ToolExecutor<ToolInvocation> for WorkflowHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(PUBLIC_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::DirectModelOnly
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl CoreToolRuntime for WorkflowHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkflowArgs {
    script: Option<String>,
    script_path: Option<String>,
    name: Option<String>,
    args: Option<Value>,
    resume_from_run_id: Option<String>,
}

pub(crate) struct JournalHandler;

impl ToolExecutor<ToolInvocation> for JournalHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(JOURNAL_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        spec::create_journal_tool()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::CodeModeOnly
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let arguments = function_arguments(invocation.payload.clone())?;
            let record: JournalAppendArgs = serde_json::from_str(&arguments).map_err(|err| {
                FunctionCallError::RespondToModel(format!("invalid workflow journal entry: {err}"))
            })?;
            let path = store::journal_path(
                &invocation.turn,
                invocation.session.thread_id,
                &record.run_id,
            )
            .map_err(FunctionCallError::RespondToModel)?;
            store::append_journal(
                path,
                JournalRecord {
                    index: record.index,
                    key: record.key,
                    value: record.value,
                },
            )
            .await
            .map_err(FunctionCallError::RespondToModel)?;
            Ok(boxed_tool_output(JsonOutput(
                serde_json::json!({ "ok": true }),
            )))
        })
    }
}

impl CoreToolRuntime for JournalHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JournalAppendArgs {
    run_id: String,
    index: u32,
    key: String,
    value: Value,
}

pub(crate) struct LoadHandler;

impl ToolExecutor<ToolInvocation> for LoadHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(LOAD_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        spec::create_load_tool()
    }

    fn exposure(&self) -> ToolExposure {
        ToolExposure::CodeModeOnly
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let arguments = function_arguments(invocation.payload.clone())?;
            let args: LoadArgs = serde_json::from_str(&arguments).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "invalid nested workflow selector: {err}"
                ))
            })?;
            let (source, _) = store::load_source(
                &invocation.turn,
                invocation.session.thread_id,
                args.name.as_deref(),
                args.script_path.as_deref(),
            )
            .await
            .map_err(FunctionCallError::RespondToModel)?;
            let parsed = store::parse_source(&source).map_err(FunctionCallError::RespondToModel)?;
            Ok(boxed_tool_output(JsonOutput(serde_json::json!({
                "meta": parsed.meta,
                "body": parsed.body,
            }))))
        })
    }
}

impl CoreToolRuntime for LoadHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LoadArgs {
    name: Option<String>,
    script_path: Option<String>,
}

struct JsonOutput(Value);

impl ToolOutput for JsonOutput {
    fn log_output(&self) -> String {
        self.0.to_string()
    }

    fn success_for_logging(&self) -> bool {
        true
    }

    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        FunctionToolOutput::from_text(self.0.to_string(), Some(true))
            .to_response_item(call_id, payload)
    }

    fn code_mode_result(&self, _payload: &ToolPayload) -> Value {
        self.0.clone()
    }
}
