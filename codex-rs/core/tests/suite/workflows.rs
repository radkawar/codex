use anyhow::Result;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;

fn function_output_text(item: &Value) -> String {
    match item.get("output") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        output => panic!("workflow output should contain text, got {output:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_tool_persists_script_and_returns_background_task_immediately() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let Ok(code_mode_host) = codex_utils_cargo_bin::cargo_bin("codex-code-mode-host") else {
        return Ok(());
    };
    let server = responses::start_mock_server().await;
    let mut builder = test_codex()
        .with_model("test-gpt-5.1-codex")
        .with_code_mode_host_program(code_mode_host)
        .with_config(|config| {
            for feature in [
                Feature::CodeMode,
                Feature::Collab,
                Feature::MultiAgentV2,
                Feature::Workflows,
            ] {
                config
                    .features
                    .enable(feature)
                    .expect("workflow dependency should be enabled");
            }
        });
    let test = builder.build_with_auto_env(&server).await?;
    let script = r#"export const meta = {
  name: 'background-check',
  description: 'Exercise the workflow runtime',
  phases: [{ title: 'Wait', detail: 'Remain live after yielding' }],
}
phase('Wait')
await new Promise(() => {})
"#;
    let arguments = serde_json::to_string(&serde_json::json!({ "script": script }))?;
    let workflow_call = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call("workflow-call", "workflow", &arguments),
            ev_completed("resp-1"),
        ]),
    )
    .await;
    let completion = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_assistant_message("msg-1", "workflow started"),
            ev_completed("resp-2"),
        ]),
    )
    .await;

    test.submit_turn("start the workflow").await?;

    let first_body = workflow_call.single_request().body_json();
    let tool_names = first_body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert!(
        tool_names.contains(&"workflow"),
        "workflow should be model-visible; tools were {tool_names:?}"
    );
    let output = function_output_text(
        &completion
            .single_request()
            .function_call_output("workflow-call"),
    );
    assert!(
        output.contains("Script running with cell ID"),
        "workflow should yield in the background; output was {output:?}"
    );
    let task: Value = output
        .lines()
        .find_map(|line| serde_json::from_str(line).ok())
        .expect("workflow output should include task metadata");
    assert_eq!(task["name"], "background-check");
    let script_path = task["scriptPath"]
        .as_str()
        .expect("task metadata should include the persisted script path");
    assert_eq!(std::fs::read_to_string(script_path)?, script);
    Ok(())
}
