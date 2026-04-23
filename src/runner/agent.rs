// =============================================================================
// runner/agent.rs — Agent subprocess adapters
//
// This module bridges Conductor's internal types to the actual CLI tools:
//
//   AgentExecutor trait     — the pluggable interface (real or mock)
//   SubprocessAgentExecutor — spawns `claude` or `codex` as child processes
//   ClaudeAdapter           — builds and runs the `claude -p ...` command
//   CodexAdapter            — builds and runs the `codex exec ...` command
//
// Both adapters communicate via structured JSON schemas:
//   • Conductor writes a JSON Schema file to a temp path.
//   • The CLI tool is told to validate its output against that schema.
//   • Conductor reads the JSON output and deserializes it into Rust types.
//
// This means the agent is *constrained* to return machine-readable output,
// not free-form text — which is what makes automation reliable.
// =============================================================================

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH}; // used to generate unique temp-file names

// `serde_json::Value` is a fully dynamic JSON type — useful when you don't know
// the shape of the data at compile time (the schema is built at runtime).
use serde_json::{Value, json};
use tracing::debug;

use crate::error::WorkflowError;
use crate::workflow::{ConductorAgent, ReviewGate, Step, Workflow};

use super::{
    ExecutionContext, ReviewDecision, StepAgentResult, expand_step_prompt, provider_for_step,
};

// ─── Public trait ─────────────────────────────────────────────────────────────

/// The interface that runner/mod.rs uses to invoke agents.
///
/// Defining this as a trait (rather than calling subprocess code directly)
/// allows the test suite to inject a `MockAgentExecutor` without touching
/// any real processes.  This is the "dependency injection" pattern.
pub trait AgentExecutor {
    /// Invoke an agent to perform a workflow step.
    fn run_step(
        &self,
        workflow: &Workflow,
        step: &Step,
        context: &ExecutionContext,
    ) -> Result<StepAgentResult, WorkflowError>;

    /// Invoke an agent to review a completed step.
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

// ─── Real subprocess executor ─────────────────────────────────────────────────

/// The production implementation — spawns `claude` or `codex` subprocesses.
/// `#[derive(Default)]` means `SubprocessAgentExecutor::default()` creates
/// a zero-field instance (no configuration needed).
#[derive(Default)]
pub struct SubprocessAgentExecutor;

impl AgentExecutor for SubprocessAgentExecutor {
    fn run_step(
        &self,
        workflow: &Workflow,
        step: &Step,
        context: &ExecutionContext,
    ) -> Result<StepAgentResult, WorkflowError> {
        // Build the full prompt string (expands snippets + appends handoff context).
        let prompt = build_step_prompt(workflow, step, context);

        // Find which provider (Claude / Codex) this step uses.
        let provider = provider_for_step(workflow, step)?;

        // Define the JSON Schema the agent must conform to.
        // `json!({...})` is a macro that builds a serde_json::Value inline.
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

        // Dispatch to the correct CLI adapter and get back raw JSON text.
        let output = invoke_provider(
            provider,
            workflow,
            step.agent.as_str(),
            Some(step),
            context,
            &prompt,
            &schema,
        )?;

        // Deserialize the raw JSON string into a StepAgentResult.
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
        // Look up the reviewer's config so we can pass it to invoke_provider.
        let reviewer = workflow.agents.get(reviewer_agent).ok_or_else(|| {
            WorkflowError::Execution(format!("unknown reviewer agent `{reviewer_agent}`"))
        })?;

        let prompt = build_review_prompt(gate, step, reviewer_agent, context, step_result);

        // Simpler schema for review decisions — just approved, summary, issues.
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
            None, // no Step context for a review invocation
            context,
            &prompt,
            &schema,
        )?;

        parse_review_decision(&output, &step.id, &gate.id)
    }
}

// ─── Provider dispatch ────────────────────────────────────────────────────────

/// Write the schema to a temp file, invoke the correct CLI adapter,
/// delete the temp file, and return the raw JSON string output.
fn invoke_provider(
    provider: &ConductorAgent,
    workflow: &Workflow,
    agent_name: &str,
    step: Option<&Step>,
    context: &ExecutionContext,
    prompt: &str,
    schema: &Value,
) -> Result<String, WorkflowError> {
    // The schema must live on disk because both Claude and Codex CLIs read it
    // from a file path rather than accepting JSON inline.
    let schema_path = write_schema_file(agent_name, schema)?;

    let output = match provider {
        ConductorAgent::ClaudeCode => {
            ClaudeAdapter::invoke(workflow, agent_name, step, context, prompt, &schema_path)
        }
        ConductorAgent::OpenAiCodex => {
            CodexAdapter::invoke(workflow, agent_name, step, context, prompt, &schema_path)
        }
    };

    // Clean up the temp schema file regardless of success or failure.
    // `let _ =` discards the Result — if the delete fails, we don't care.
    let _ = fs::remove_file(&schema_path);

    output
}

// ─── Claude Code adapter ──────────────────────────────────────────────────────

struct ClaudeAdapter;

impl ClaudeAdapter {
    /// Invoke the `claude` CLI in print mode with structured JSON output.
    ///
    /// Key flags:
    ///   `-p`                — print mode (non-interactive, reads prompt from arg)
    ///   `--output-format json` — return a JSON envelope
    ///   `--json-schema`     — constrain output to the supplied schema
    ///   `--permission-mode` — controls what filesystem actions Claude can take
    fn invoke(
        workflow: &Workflow,
        agent_name: &str,
        step: Option<&Step>,
        context: &ExecutionContext,
        prompt: &str,
        schema_path: &Path,
    ) -> Result<String, WorkflowError> {
        // Allow overriding the binary path — handy for testing with a stub.
        let binary = env::var("CONDUCTOR_CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());

        let agent = workflow
            .agents
            .get(agent_name)
            .ok_or_else(|| WorkflowError::Execution(format!("unknown agent `{agent_name}`")))?;

        // Read the schema back from disk — Claude wants the schema text, not the path.
        let schema = fs::read_to_string(schema_path).map_err(|error| {
            WorkflowError::Execution(format!(
                "failed to read generated schema `{}`: {}",
                schema_path.display(),
                error
            ))
        })?;

        // Build the subprocess command incrementally.
        let mut command = Command::new(&binary);
        command
            .arg("-p")
            .arg("--output-format")
            .arg("json")
            .arg("--permission-mode")
            .arg(claude_permission_mode()) // "acceptEdits" by default
            .arg("--json-schema")
            .arg(schema)
            .arg(format!("--add-dir={}", context.workspace_root.display()))
            .arg(prompt)
            .current_dir(&context.workspace_root)
            // Inject any per-agent environment variables from the workflow config.
            .envs(agent.env.iter());

        // Optional model override (e.g. "sonnet", "opus").
        if let Some(model) = agent.model.as_deref() {
            command.arg("--model").arg(model);
        }

        debug!(
            "running Claude adapter in {}",
            context.workspace_root.display()
        );

        // `.output()` waits for the process to finish and captures stdout/stderr.
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

        // Claude wraps structured output in an envelope; normalize it to bare JSON.
        normalize_claude_output(&String::from_utf8_lossy(&output.stdout), step_id(step))
    }
}

// ─── OpenAI Codex adapter ─────────────────────────────────────────────────────

struct CodexAdapter;

impl CodexAdapter {
    /// Invoke the `codex exec` CLI, writing structured output to a temp file.
    ///
    /// Codex doesn't stream JSON to stdout like Claude — instead it writes its
    /// last-message output to a file path we provide via `--output-last-message`.
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

        // Codex writes the response JSON here; we read it after the process exits.
        let output_path = temp_file_path("codex-output", "json");

        let mut command = Command::new(&binary);
        command
            .arg("exec")
            .arg("--ephemeral")           // don't persist a session
            .arg("--skip-git-repo-check") // don't require a git repo
            .arg("--sandbox")
            .arg(codex_sandbox_mode())    // "workspace-write" by default
            .arg("--color")
            .arg("never")
            .arg("--cd")
            .arg(&context.workspace_root)
            .arg("--output-schema")
            .arg(schema_path)
            .arg("--output-last-message")
            .arg(&output_path) // where Codex writes its JSON output
            .arg(prompt)
            .envs(agent.env.iter());

        // `--full-auto` skips manual approval prompts inside Codex.
        if codex_full_auto_enabled() {
            command.arg("--full-auto");
        }

        if let Some(model) = agent.model.as_deref() {
            command.arg("--model").arg(model);
        }

        // Optional profile (e.g. "reviewer") forwarded from the workflow.
        if let Ok(profile) = env::var("CONDUCTOR_CODEX_PROFILE") {
            command.arg("--profile").arg(profile);
        }

        // Allow overriding Codex's home directory (useful when /tmp is not writable).
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

        // Read the output file Codex wrote, then delete it.
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

// ─── Prompt builders ──────────────────────────────────────────────────────────

/// Build the full prompt for a step execution, including context metadata,
/// shell commands the agent should be aware of, and expanded snippet text.
fn build_step_prompt(workflow: &Workflow, step: &Step, context: &ExecutionContext) -> String {
    let mut prompt = String::new();

    // Header tells the agent what kind of structured response is expected.
    prompt.push_str("You are executing a Conductor workflow step.\n");
    prompt.push_str("Return only JSON that matches the provided schema.\n\n");

    // Inject context the agent needs to understand its situation.
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

    // Let the agent know which shell commands Conductor will run after it succeeds.
    // This helps it write code that will pass those checks.
    if !step.run.is_empty() {
        prompt.push_str("Shell commands Conductor will execute after a successful agent run:\n");
        for command in &step.run {
            prompt.push_str(&format!("- {command}\n"));
        }
        prompt.push('\n');
    }

    // Expand snippet triggers and append named snippets + handoff context.
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

/// Build the prompt for a review gate invocation.
fn build_review_prompt(
    gate: &ReviewGate,
    step: &Step,
    reviewer_agent: &str,
    context: &ExecutionContext,
    step_result: &StepAgentResult,
) -> String {
    // Format open questions as a Markdown list for readability.
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

    // `format!` with `\n\` continues the string on the next source line
    // without inserting an actual newline in the string value.
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

// ─── Schema temp-file helpers ─────────────────────────────────────────────────

/// Serialize the schema to a uniquely-named temp file and return its path.
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

/// Generate a unique temp-file path using a nanosecond timestamp as a nonce.
/// Using a nonce rather than a fixed name prevents collisions when multiple
/// Conductor processes run in parallel.
fn temp_file_path(prefix: &str, extension: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    env::temp_dir().join(format!("{prefix}-{nonce}.{extension}"))
}

// ─── Output parsers ───────────────────────────────────────────────────────────

/// Parse raw JSON text into a `StepAgentResult`.
///
/// Tries a direct parse first; falls back to extracting the first `{...}` block
/// in case the agent included prose before or after the JSON.
fn parse_agent_result(output: &str, step_id: &str) -> Result<StepAgentResult, WorkflowError> {
    serde_json::from_str(output)
        .or_else(|_| extract_embedded_json(output).and_then(|json| serde_json::from_str(&json)))
        .map_err(|error| WorkflowError::StepFailed {
            step_id: step_id.to_string(),
            message: format!("agent returned invalid JSON: {error}. raw output: {output}"),
        })
}

/// Parse raw JSON text into a `ReviewDecision`.
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

/// Extract the first `{...}` substring from a string.
///
/// Some agent outputs include prose like "Here is my response: {...}".
/// `find` and `rfind` locate the outermost braces; the slice in between is
/// the JSON object we care about.
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
    // `start..=end` is an inclusive range — includes the `}` character itself.
    Ok(output[start..=end].to_string())
}

/// Claude wraps structured output in a JSON envelope with a `structured_output`
/// key.  This function extracts just the inner object, or falls back to the
/// `result` string field, or returns the raw text as a last resort.
fn normalize_claude_output(output: &str, step_id: &str) -> Result<String, WorkflowError> {
    let value: Value = serde_json::from_str(output).map_err(|error| WorkflowError::StepFailed {
        step_id: step_id.to_string(),
        message: format!("Claude returned invalid JSON envelope: {error}. raw output: {output}"),
    })?;

    // Path 1: `{"structured_output": {...}}` — the normal case with --json-schema.
    if let Some(structured) = value.get("structured_output") {
        return serde_json::to_string(structured).map_err(|error| WorkflowError::StepFailed {
            step_id: step_id.to_string(),
            message: format!("failed to serialize Claude structured output: {error}"),
        });
    }

    // Path 2: `{"result": "..."}` — plain text result in a JSON wrapper.
    if let Some(result) = value.get("result").and_then(Value::as_str) {
        return Ok(result.to_string());
    }

    // Path 3: treat the whole output as the JSON to parse.
    Ok(output.trim().to_string())
}

// ─── Environment-variable helpers ────────────────────────────────────────────

fn format_codex_failure(status: std::process::ExitStatus, stdout: &str, stderr: &str) -> String {
    let combined = format!("{stdout}\n{stderr}");

    // Detect authentication failure and return a helpful message instead of raw logs.
    if combined.contains("401 Unauthorized") {
        return "Codex CLI authentication failed (401 Unauthorized). Run `codex login`, provide `OPENAI_API_KEY`, or verify the active Codex profile before using `conductor run --live`.".to_string();
    }

    // Detect a read-only session store — common in restricted environments.
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
    // "acceptEdits" lets Claude read and write files without asking each time.
    env::var("CONDUCTOR_CLAUDE_PERMISSION_MODE").unwrap_or_else(|_| "acceptEdits".to_string())
}

fn codex_sandbox_mode() -> String {
    // "workspace-write" allows writes inside the workspace but not elsewhere.
    env::var("CONDUCTOR_CODEX_SANDBOX").unwrap_or_else(|_| "workspace-write".to_string())
}

fn codex_full_auto_enabled() -> bool {
    // Defaults to true (full-auto) unless explicitly disabled via env var.
    env::var("CONDUCTOR_CODEX_FULL_AUTO")
        .map(|value| value != "0" && value.to_lowercase() != "false")
        .unwrap_or(true)
}

/// Helper: get a step ID string for error messages, falling back to "review"
/// when the context doesn't have a step (i.e. during a review invocation).
fn step_id(step: Option<&Step>) -> &str {
    step.map(|item| item.id.as_str()).unwrap_or("review")
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn extracts_claude_structured_output() {
        // Simulate the JSON envelope Claude returns with --output-format json --json-schema.
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
        // On non-Unix platforms produce any non-success status by running a failing command.
        #[cfg(not(unix))]
        let status = Command::new("true").status().expect("status");

        let message = format_codex_failure(status, "", "HTTP error: 401 Unauthorized");
        assert!(message.contains("codex login"));
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
// • Trait objects (`Box<dyn AgentExecutor>`) decouple the runner from the
//   concrete subprocess code — tests swap in a mock without changing any
//   runner logic.  This is the "strategy pattern" in Rust.
// • `Command::new(...).arg(...).output()` — spawns a child process and
//   waits for it to finish, capturing stdout and stderr as byte vectors.
// • `String::from_utf8_lossy(&bytes)` — converts raw bytes to a string,
//   replacing invalid UTF-8 sequences with U+FFFD (safer than from_utf8).
// • `.or_else(|_| ...)` on Result — try a fallback operation if the first
//   one fails; the `|_|` discards the error from the first attempt.
// • Nanosecond timestamp as a nonce — cheap uniqueness guarantee for temp
//   files that doesn't require a UUID crate.
