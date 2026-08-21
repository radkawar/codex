use codex_tools::AdditionalProperties;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use std::collections::BTreeMap;

pub(super) const PUBLIC_TOOL_NAME: &str = "workflow";
pub(super) const JOURNAL_TOOL_NAME: &str = "workflow_journal_append";
pub(super) const LOAD_TOOL_NAME: &str = "workflow_load";

pub(super) fn create_workflow_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "script".to_string(),
            JsonSchema::string(Some(
                "A plain JavaScript workflow beginning with a pure-literal `export const meta = { ... }`.".to_string(),
            )),
        ),
        (
            "scriptPath".to_string(),
            JsonSchema::string(Some(
                "A path returned by an earlier workflow call, or a saved workflow path under `.codex/workflows` or `$CODEX_HOME/workflows`.".to_string(),
            )),
        ),
        (
            "name".to_string(),
            JsonSchema::string(Some(
                "The name of a saved workflow in `.codex/workflows` or `$CODEX_HOME/workflows`.".to_string(),
            )),
        ),
        (
            "args".to_string(),
            JsonSchema {
                description: Some("Any real JSON value passed to the workflow as `args`.".to_string()),
                ..Default::default()
            },
        ),
        (
            "resumeFromRunId".to_string(),
            JsonSchema::string(Some(
                "Resume a prior run. The longest unchanged prefix of `agent()` calls is restored from its journal.".to_string(),
            )),
        ),
    ]);
    ToolSpec::Function(ResponsesApiTool {
        name: PUBLIC_TOOL_NAME.to_string(),
        description: "Run a deterministic JavaScript program that coordinates Codex subagents. The call yields immediately with a task ID and script path, then posts progress and completion notifications. Use only after explicit user opt-in: a direct workflow/fan-out request or a named saved workflow.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ None,
            Some(AdditionalProperties::Boolean(false)),
        ),
        output_schema: None,
    })
}

pub(super) fn create_journal_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: JOURNAL_TOOL_NAME.to_string(),
        description: "Internal workflow runtime hook for persisting completed agent calls."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                ("runId".to_string(), JsonSchema::string(None)),
                ("index".to_string(), JsonSchema::integer(None)),
                ("key".to_string(), JsonSchema::string(None)),
                (
                    "value".to_string(),
                    JsonSchema {
                        ..Default::default()
                    },
                ),
            ]),
            Some(vec![
                "runId".to_string(),
                "index".to_string(),
                "key".to_string(),
                "value".to_string(),
            ]),
            Some(AdditionalProperties::Boolean(false)),
        ),
        output_schema: None,
    })
}

pub(super) fn create_load_tool() -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: LOAD_TOOL_NAME.to_string(),
        description: "Internal workflow runtime hook for loading one nested saved workflow."
            .to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            BTreeMap::from([
                ("name".to_string(), JsonSchema::string(None)),
                ("scriptPath".to_string(), JsonSchema::string(None)),
            ]),
            /*required*/ None,
            Some(AdditionalProperties::Boolean(false)),
        ),
        output_schema: None,
    })
}
