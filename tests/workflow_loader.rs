use std::fs;

use conductor::workflow::load_workflow;
use tempfile::tempdir;

#[test]
fn loads_valid_workflow() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("workflow.yaml");

    fs::write(
        &path,
        r#"
version: "1"
name: "test-workflow"
agents:
  claude:
    provider: "claude_code"
steps:
  - id: "step_1"
    title: "First step"
    agent: "claude"
"#,
    )
    .expect("write workflow");

    let workflow = load_workflow(&path).expect("workflow should load");
    assert_eq!(workflow.name, "test-workflow");
    assert_eq!(workflow.steps.len(), 1);
}

#[test]
fn rejects_unknown_step_agent() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("workflow.yaml");

    fs::write(
        &path,
        r#"
version: "1"
name: "bad-workflow"
agents:
  codex:
    provider: "open_ai_codex"
steps:
  - id: "step_1"
    title: "First step"
    agent: "claude"
"#,
    )
    .expect("write workflow");

    let err = load_workflow(&path).expect_err("workflow should fail");
    let message = err.to_string();
    assert!(message.contains("unknown agent `claude`"));
}
