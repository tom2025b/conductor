use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tracing::debug;

use crate::error::WorkflowError;
use crate::workflow::{ConductorAgent, ReviewGate, Step, Workflow};

use super::{
    ExecutionContext, ReviewDecision, StepAgentResult, expand_step_prompt, provider_for_step,
};

pub trait AgentExecutor {
    fn run_step(
        &self,
        workflow: &Workflow,
        step: &Step,
        context: &ExecutionContext,
    ) -> Result<StepAgentResult, WorkflowError>;

    fn run_review(
        &self,
        workflow: &Workflow,
        gate: &ReviewGate,
        step: &Step,
        reviewer_agent: &str,
        context: &ExecutionContext,
        step_result: &StepAgentResult,
    ) -> Result<ReviewDecision, WorkflowError>;
}

#[derive(Default)]
pub struct SubprocessAgentExecutor;

impl AgentExecutor for SubprocessAgentExecutor {
    fn run_step(
        &self,
        workflow: &Workflow,
        step: &Step,
        context: &ExecutionContext,
    ) -> Result<StepAgentResult, WorkflowError> {
        let prompt = build_step_prompt(workflow, step, context);
        let provider = provider_for_step(workflow, step)?;
        let schema = json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["success", "failure"] },
                "summary": { "type": "string" },
                "current_state": { "type": ["string", "null"] },
                "open_questions": {
                    "type": "array",
                    "items": { "type": "string" }
                },
                "next_actions": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": ["status", "summary", "current_state", "open_questions", "next_actions"],
            "additionalProperties": false
        });

        let output = invoke_provider(
            provider,
            workflow,
            step.agent.as_str(),
            Some(step),
            context,
            &prompt,
            &schema,
        )?;

        parse_agent_result(&output, &step.id)
    }

    fn run_review(
        &self,
        workflow: &Workflow,
        gate: &ReviewGate,
        step: &Step,
        reviewer_agent: &str,
        context: &ExecutionContext,
        step_result: &StepAgentResult,
    ) -> Result<ReviewDecision, WorkflowError> {
        let reviewer = workflow.agents.get(reviewer_agent).ok_or_else(|| {
            WorkflowError::Execution(format!("unknown reviewer agent `{reviewer_agent}`"))
        })?;

        let prompt = build_review_prompt(gate, step, reviewer_agent, context, step_result);
        let schema = json!({
            "type": "object",
            "properties": {
                "approved": { "type": "boolean" },
                "summary": { "type": "string" },
                "blocking_issues": {
                    "type": "array",
                    "items": { "type": "string" }
                }
            },
            "required": ["approved", "summary", "blocking_issues"],
            "additionalProperties": false
        });

        let output = invoke_provider(
            &reviewer.provider,
            workflow,
            reviewer_agent,
            None,
            context,
            &prompt,
            &schema,
        )?;

        parse_review_decision(&output, &step.id, &gate.id)
    }
}

fn invoke_provider(
    provider: &ConductorAgent,
    workflow: &Workflow,
    agent_name: &str,
    step: Option<&Step>,
    context: &ExecutionContext,
    prompt: &str,
    schema: &Value,
) -> Result<String, WorkflowError> {
    let schema_path = write_schema_file(agent_name, schema)?;
    let output = match provider {
        ConductorAgent::ClaudeCode => {
            ClaudeAdapter::invoke(workflow, agent_name, step, context, prompt, &schema_path)
        }
        ConductorAgent::OpenAiCodex => {
            CodexAdapter::invoke(workflow, agent_name, step, context, prompt, &schema_path)
        }
    };
    let _ = fs::remove_file(&schema_path);
    output
}

struct ClaudeAdapter;

impl ClaudeAdapter {
    fn invoke(
        workflow: &Workflow,
        agent_name: &str,
        step: Option<&Step>,
        context: &ExecutionContext,
        prompt: &str,
        schema_path: &Path,
    ) -> Result<String, WorkflowError> {
        let binary = env::var("CONDUCTOR_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
        let agent = workflow
            .agents
            .get(agent_name)
            .ok_or_else(|| WorkflowError::Execution(format!("unknown agent `{agent_name}`")))?;
        let schema = fs::read_to_string(schema_path).map_err(|error| {
            WorkflowError::Execution(format!(
                "failed to read generated schema `{}`: {}",
                schema_path.display(),
                error
            ))
        })?;

        let mut command = Command::new(&binary);
        command
            .arg("-p")
            .arg("--output-format")
            .arg("json")
            .arg("--permission-mode")
            .arg(claude_permission_mode())
            .arg("--json-schema")
            .arg(schema)
            .arg(format!("--add-dir={}", context.workspace_root.display()))
            .arg(prompt)
            .current_dir(&context.workspace_root)
            .envs(agent.env.iter());

        if let Some(model) = agent.model.as_deref() {
            command.arg("--model").arg(model);
        }

        debug!(
            "running Claude adapter in {}",
            context.workspace_root.display()
        );

        let output = command
            .output()
            .map_err(|error| WorkflowError::StepFailed {
                step_id: step_id(step).to_string(),
                message: format!("failed to start Claude CLI `{binary}`: {error}"),
            })?;

        if !output.status.success() {
            return Err(WorkflowError::StepFailed {
                step_id: step_id(step).to_string(),
                message: format!(
                    "Claude CLI exited with status {}.\nstdout:\n{}\nstderr:\n{}",
                    output.status,
                    String::from_utf8_lossy(&output.stdout).trim(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }

        normalize_claude_output(&String::from_utf8_lossy(&output.stdout), step_id(step))
    }
}

struct CodexAdapter;

impl CodexAdapter {
    fn invoke(
        workflow: &Workflow,
        agent_name: &str,
        step: Option<&Step>,
        context: &ExecutionContext,
        prompt: &str,
        schema_path: &Path,
    ) -> Result<String, WorkflowError> {
        let binary = env::var("CONDUCTOR_CODEX_BIN").unwrap_or_else(|_| "codex".to_string());
        let agent = workflow
            .agents
            .get(agent_name)
            .ok_or_else(|| WorkflowError::Execution(format!("unknown agent `{agent_name}`")))?;
        let output_path = temp_file_path("codex-output", "json");

        let mut command = Command::new(&binary);
        command
            .arg("exec")
            .arg("--ephemeral")
            .arg("--skip-git-repo-check")
            .arg("--sandbox")
            .arg(codex_sandbox_mode())
            .arg("--color")
            .arg("never")
            .arg("--cd")
            .arg(&context.workspace_root)
            .arg("--output-schema")
            .arg(schema_path)
            .arg("--output-last-message")
            .arg(&output_path)
            .arg(prompt)
            .envs(agent.env.iter());

        if codex_full_auto_enabled() {
            command.arg("--full-auto");
        }

        if let Some(model) = agent.model.as_deref() {
            command.arg("--model").arg(model);
        }

        if let Ok(profile) = env::var("CONDUCTOR_CODEX_PROFILE") {
            command.arg("--profile").arg(profile);
        }

        if let Ok(path) = env::var("CONDUCTOR_CODEX_HOME") {
            command.env("CODEX_HOME", path);
        }

        debug!(
            "running Codex adapter in {}",
            context.workspace_root.display()
        );

        let output = command
            .output()
            .map_err(|error| WorkflowError::StepFailed {
                step_id: step_id(step).to_string(),
                message: format!("failed to start Codex CLI `{binary}`: {error}"),
            })?;

        if !output.status.success() {
            return Err(WorkflowError::StepFailed {
                step_id: step_id(step).to_string(),
                message: format_codex_failure(
                    output.status,
                    &String::from_utf8_lossy(&output.stdout),
                    &String::from_utf8_lossy(&output.stderr),
                ),
            });
        }

        let result = fs::read_to_string(&output_path).map_err(|error| {
            WorkflowError::Execution(format!(
                "failed to read Codex output file `{}`: {}",
                output_path.display(),
                error
            ))
        });
        let _ = fs::remove_file(&output_path);
        result.map(|body| body.trim().to_string())
    }
}

fn build_step_prompt(workflow: &Workflow, step: &Step, context: &ExecutionContext) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are executing a Conductor workflow step.\n");
    prompt.push_str("Return only JSON that matches the provided schema.\n\n");
    prompt.push_str(&format!("Workflow: {}\n", workflow.name));
    prompt.push_str(&format!("Step ID: {}\n", step.id));
    prompt.push_str(&format!("Step Title: {}\n", step.title));
    prompt.push_str(&format!("Agent Name: {}\n", step.agent));
    prompt.push_str(&format!(
        "Workspace Root: {}\n",
        context.workspace_root.display()
    ));
    prompt.push_str(&format!(
        "Handoff File: {}\n",
        context.handoff_path.display()
    ));

    if !step.run.is_empty() {
        prompt.push_str("Shell commands Conductor will execute after a successful agent run:\n");
        for command in &step.run {
            prompt.push_str(&format!("- {command}\n"));
        }
        prompt.push('\n');
    }

    let expanded = expand_step_prompt(workflow, step, &context.handoff_markdown);
    if !expanded.trim().is_empty() {
        prompt.push_str("Step Instructions:\n");
        prompt.push_str(&expanded);
        prompt.push('\n');
    }

    prompt.push_str(
        "\nPerform the requested work in the workspace, then report the outcome, current state for the next handoff, any open questions, and the next actions.",
    );
    prompt
}

fn build_review_prompt(
    gate: &ReviewGate,
    step: &Step,
    reviewer_agent: &str,
    context: &ExecutionContext,
    step_result: &StepAgentResult,
) -> String {
    let issues = if step_result.open_questions.is_empty() {
        "- None".to_string()
    } else {
        step_result
            .open_questions
            .iter()
            .map(|item| format!("- {item}"))
            .collect::<Vec<_>>()
            .join("\n")
    };

    format!(
        "You are executing a Conductor review gate.\n\
         Return only JSON that matches the provided schema.\n\n\
         Review Gate: {} ({})\n\
         Reviewer Agent: {}\n\
         Step ID: {}\n\
         Step Title: {}\n\
         Workspace Root: {}\n\
         Handoff File: {}\n\n\
         Gate Instructions:\n{}\n\n\
         Step Summary:\n{}\n\n\
         Current State:\n{}\n\n\
         Open Questions:\n{}\n\n\
         Decide whether the step is approved for the workflow to continue. If not approved, list concrete blocking issues.",
        gate.name,
        gate.id,
        reviewer_agent,
        step.id,
        step.title,
        context.workspace_root.display(),
        context.handoff_path.display(),
        gate.instructions
            .as_deref()
            .unwrap_or("No extra instructions."),
        step_result.summary,
        step_result
            .current_state
            .as_deref()
            .unwrap_or("No state provided."),
        issues
    )
}

fn write_schema_file(agent_name: &str, schema: &Value) -> Result<PathBuf, WorkflowError> {
    let path = temp_file_path(&format!("conductor-{agent_name}-schema"), "json");
    let body = serde_json::to_vec_pretty(schema).map_err(|error| {
        WorkflowError::Execution(format!("failed to serialize schema: {error}"))
    })?;
    fs::write(&path, body).map_err(|error| {
        WorkflowError::Execution(format!(
            "failed to write schema file `{}`: {}",
            path.display(),
            error
        ))
    })?;
    Ok(path)
}

fn temp_file_path(prefix: &str, extension: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    env::temp_dir().join(format!("{prefix}-{nonce}.{extension}"))
}

fn parse_agent_result(output: &str, step_id: &str) -> Result<StepAgentResult, WorkflowError> {
    serde_json::from_str(output)
        .or_else(|_| extract_embedded_json(output).and_then(|json| serde_json::from_str(&json)))
        .map_err(|error| WorkflowError::StepFailed {
            step_id: step_id.to_string(),
            message: format!("agent returned invalid JSON: {error}. raw output: {output}"),
        })
}

fn parse_review_decision(
    output: &str,
    step_id: &str,
    gate_id: &str,
) -> Result<ReviewDecision, WorkflowError> {
    serde_json::from_str(output)
        .or_else(|_| extract_embedded_json(output).and_then(|json| serde_json::from_str(&json)))
        .map_err(|error| WorkflowError::StepFailed {
            step_id: step_id.to_string(),
            message: format!(
                "review gate `{gate_id}` returned invalid JSON: {error}. raw output: {output}"
            ),
        })
}

fn extract_embedded_json(output: &str) -> Result<String, serde_json::Error> {
    let start = output.find('{').ok_or_else(|| {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing opening brace",
        ))
    })?;
    let end = output.rfind('}').ok_or_else(|| {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "missing closing brace",
        ))
    })?;
    Ok(output[start..=end].to_string())
}

fn normalize_claude_output(output: &str, step_id: &str) -> Result<String, WorkflowError> {
    let value: Value = serde_json::from_str(output).map_err(|error| WorkflowError::StepFailed {
        step_id: step_id.to_string(),
        message: format!("Claude returned invalid JSON envelope: {error}. raw output: {output}"),
    })?;

    if let Some(structured) = value.get("structured_output") {
        return serde_json::to_string(structured).map_err(|error| WorkflowError::StepFailed {
            step_id: step_id.to_string(),
            message: format!("failed to serialize Claude structured output: {error}"),
        });
    }

    if let Some(result) = value.get("result").and_then(Value::as_str) {
        return Ok(result.to_string());
    }

    Ok(output.trim().to_string())
}

fn format_codex_failure(status: std::process::ExitStatus, stdout: &str, stderr: &str) -> String {
    let combined = format!("{stdout}\n{stderr}");
    if combined.contains("401 Unauthorized") {
        return "Codex CLI authentication failed (401 Unauthorized). Run `codex login`, provide `OPENAI_API_KEY`, or verify the active Codex profile before using `conductor run --live`.".to_string();
    }

    if combined.contains("Read-only file system") && combined.contains("session") {
        return "Codex CLI could not initialize its local session store. Set `CONDUCTOR_CODEX_HOME` to a writable directory or run with a writable Codex home.".to_string();
    }

    format!(
        "Codex CLI exited with status {}.\nstdout:\n{}\nstderr:\n{}",
        status,
        stdout.trim(),
        stderr.trim()
    )
}

fn claude_permission_mode() -> String {
    env::var("CONDUCTOR_CLAUDE_PERMISSION_MODE").unwrap_or_else(|_| "acceptEdits".to_string())
}

fn codex_sandbox_mode() -> String {
    env::var("CONDUCTOR_CODEX_SANDBOX").unwrap_or_else(|_| "workspace-write".to_string())
}

fn codex_full_auto_enabled() -> bool {
    env::var("CONDUCTOR_CODEX_FULL_AUTO")
        .map(|value| value != "0" && value.to_lowercase() != "false")
        .unwrap_or(true)
}

fn step_id(step: Option<&Step>) -> &str {
    step.map(|item| item.id.as_str()).unwrap_or("review")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn extracts_claude_structured_output() {
        let raw = r#"{"type":"result","structured_output":{"status":"success","summary":"done","current_state":"state","open_questions":[],"next_actions":["next"]}}"#;
        let normalized = normalize_claude_output(raw, "step_1").expect("normalize");
        let parsed: StepAgentResult = serde_json::from_str(&normalized).expect("parse");
        assert_eq!(parsed.summary, "done");
        assert_eq!(parsed.next_actions, vec!["next"]);
    }

    #[test]
    fn formats_codex_auth_failure() {
        #[cfg(unix)]
        let status = std::process::ExitStatus::from_raw(256);
        #[cfg(not(unix))]
        let status = Command::new("true").status().expect("status");

        let message = format_codex_failure(status, "", "HTTP error: 401 Unauthorized");
        assert!(message.contains("codex login"));
    }
}
