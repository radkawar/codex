use super::*;
use pretty_assertions::assert_eq;

#[test]
fn parses_javascript_literal_meta_and_body() {
    let parsed = parse(
        r#"export const meta = {
            name: 'review',
            description: "Review code", // shown in the permission dialog
            whenToUse: 'when asked',
            phases: [
                { title: 'Scan', detail: 'Find candidates' },
                { title: 'Verify', detail: 'Check candidates' },
            ],
        };
        phase('Scan')
        "#,
    )
    .expect("valid workflow");

    assert_eq!(parsed.meta.name, "review");
    assert_eq!(parsed.meta.phases[1].title, "Verify");
    assert!(parsed.body.contains("phase('Scan')"));
}

#[test]
fn rejects_non_literal_meta_values() {
    let error = parse("export const meta = { name: makeName(), description: 'x', phases: [] };")
        .expect_err("function call must fail");
    assert!(error.contains("pure literal"));
}

#[test]
fn rejects_duplicate_phase_titles() {
    let error = parse(
        "export const meta = { name: 'x', description: 'x', phases: [{title: 'A', detail: ''}, {title: 'A', detail: ''}] };",
    )
    .expect_err("duplicate phase must fail");
    assert!(error.contains("declared more than once"));
}
