use super::*;
use crate::tools::workflows::parser::WorkflowMeta;
use crate::tools::workflows::parser::WorkflowPhase;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn embeds_inputs_as_json_without_interpreting_source() {
    let meta = WorkflowMeta {
        name: "quoted\"name".to_string(),
        description: "description".to_string(),
        when_to_use: None,
        phases: vec![WorkflowPhase {
            title: "Scan".to_string(),
            detail: "detail".to_string(),
        }],
    };
    let source = build(RuntimeInput {
        run_id: "run-1",
        script_path: "/tmp/run-1.js",
        body: "log(`body ${args.value}`)",
        meta: &meta,
        args: &json!({"value": "x"}),
        journal: &[],
        concurrency: 7,
    })
    .expect("runtime source");

    assert!(source.contains(r#"const __workflowName = "quoted\"name";"#));
    assert!(source.contains(r#"const __workflowBody = "log(`body ${args.value}`)";"#));
    assert!(source.contains("const __workflowConcurrency = 7;"));
    assert_eq!(source.matches("__CODEX_").count(), 0);
}
