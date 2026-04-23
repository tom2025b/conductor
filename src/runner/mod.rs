// =============================================================================
// runner/mod.rs — Workflow execution engine
//
// This is the heart of Conductor.  The `Runner` struct takes a validated
// Workflow and drives it step by step:
//
//   for each step (up to max_steps):
//     1. Read the handoff file so the agent has full context.
//     2. Call the agent executor (real subprocess or dry-run stub).
//     3. If the step succeeded and review is required, run a review gate.
//     4. Decide the next step based on the transition rules (continue / retry / halt).
//   return an ExecutionReport
//
// Two sub-modules live alongside this file:
//   agent.rs    — knows how to spawn claude/codex subprocesses
//   handoff.rs  — manages reading/writing the handoff Markdown file
// =============================================================================

// Private sub-modules — only this file can use them directly.
mod agent;
mod handoff;

use std::path::{Path, PathBuf};
use std::process::Command; // used to run shell commands in `step.run`

use serde::{Deserialize, Serialize};
use tracing::{info, warn}; // structured logging macros

use crate::error::WorkflowError;
use crate::workflow::{ConductorAgent, HandoffMode, Step, StepRoute, StepTransition, Workflow};

// Bring in the two sub-module types we use internally.
use self::agent::{AgentExecutor, SubprocessAgentExecutor};
use self::handoff::HandoffStore;

// ─── Public types ─────────────────────────────────────────────────────────────

/// The main orchestrator.  Owns the workflow definition and drives execution.
pub struct Runner {
    workflow: Workflow,

    /// Directory that workflow-relative paths (handoff file, workspaces) are resolved against.
    workspace_root: PathBuf,

    options: RunnerOptions,

    /// The pluggable agent execution backend.
    /// `Box<dyn AgentExecutor>` is a trait object — the concrete type is chosen at
    /// construction time (real subprocess in production, mock in tests).
    agent_executor: Box<dyn AgentExecutor>,
}

/// Tuning knobs for a single run.
#[derive(Debug, Clone)]
pub struct RunnerOptions {
    /// When true, agents are not actually invoked; steps produce a fake "success" result.
    pub dry_run: bool,

    /// Hard ceiling on the number of steps to prevent infinite retry loops.
    pub max_steps: usize,
}

/// A full record of what happened during a workflow run, suitable for display or logging.
#[derive(Debug, Clone)]
pub struct ExecutionReport {
    pub workflow_name: String,
    pub dry_run: bool,
    pub completed_steps: Vec<CompletedStep>,
    pub final_status: RunStatus,
}

/// The outcome of the entire workflow.
#[derive(Debug, Clone)]
pub enum RunStatus {
    /// Last step ran and there was no `next_step` to advance to.
    Completed,
    /// A step had `route: halt` — this is a deliberate, clean stop.
    Halted,
    /// An error occurred — the String contains a description.
    Failed(String),
}

/// Summary of one step that has finished (used in ExecutionReport).
#[derive(Debug, Clone)]
pub struct CompletedStep {
    pub step_id: String,
    pub agent: String,
    pub reviewed: bool,
    pub status: StepExecutionStatus,
}

/// How a specific step ended.
#[derive(Debug, Clone)]
pub enum StepExecutionStatus {
    Succeeded,
    Failed,
    DryRun, // step was skipped because dry_run = true
}

/// The structured JSON response an agent returns after completing a step.
/// `serde` derives let us parse this directly from JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepAgentResult {
    pub status: AgentRunStatus,
    pub summary: String,

    // `#[serde(default)]` means if the field is absent in the JSON, use None / Vec::new().
    #[serde(default)]
    pub current_state: Option<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub next_actions: Vec<String>,
}

/// The two values an agent can return for its `status` field.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")] // "success" / "failure" in JSON
pub enum AgentRunStatus {
    Success,
    Failure,
}

/// The structured JSON response a review agent returns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDecision {
    pub approved: bool,
    pub summary: String,
    #[serde(default)]
    pub blocking_issues: Vec<String>,
}

/// All the runtime information available when a step is executing.
/// Passed to the agent executor so it can build the subprocess prompt.
#[derive(Debug, Clone)]
pub struct ExecutionContext {
    pub current_step_id: String,
    pub current_agent: String,
    pub workspace_root: PathBuf,
    pub handoff_path: PathBuf,
    /// The full text of the handoff file at the time this step started.
    pub handoff_markdown: String,
}

// ─── Runner implementation ────────────────────────────────────────────────────

impl Runner {
    /// Construct a Runner for production use (real subprocess executor).
    pub fn new(workflow: Workflow, workflow_path: &Path, options: RunnerOptions) -> Self {
        // The workspace root is the directory that *contains* the workflow file.
        // If the workflow file has no parent (e.g. it's just "workflow.yaml"),
        // fall back to the current directory.
        let workspace_root = workflow_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            workflow,
            workspace_root,
            options,
            // `Box::new(...)` heap-allocates the executor and stores it as a trait object.
            agent_executor: Box::new(SubprocessAgentExecutor::default()),
        }
    }

    /// Construct a Runner with a custom executor — only compiled into test builds.
    #[cfg(test)]
    pub fn with_agent_executor(
        workflow: Workflow,
        workflow_path: &Path,
        options: RunnerOptions,
        agent_executor: Box<dyn AgentExecutor>,
    ) -> Self {
        let workspace_root = workflow_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        Self {
            workflow,
            workspace_root,
            options,
            agent_executor,
        }
    }

    /// Execute the workflow and return a report of everything that happened.
    ///
    /// # How it works
    ///
    /// This is a bounded loop (max_steps) that drives a state machine:
    ///   current_step_id → execute → review? → transition → next step
    ///
    /// The loop exits early when a step returns `route: halt`, when there is no
    /// next step to advance to, or when max_steps is reached.
    pub fn run(&self) -> Result<ExecutionReport, WorkflowError> {
        // The HandoffStore manages reading and writing the handoff Markdown file.
        let mut handoff_store = HandoffStore::new(
            self.workspace_root.join(&self.workflow.handoff.path),
            self.workflow.handoff.mode.clone(),
            self.workflow.handoff.required_sections.clone(),
        );

        let mut completed_steps = Vec::new();

        // Start at the first step; transitions update this variable each iteration.
        let mut current_step_id = self
            .workflow
            .steps
            .first()
            .map(|step| step.id.clone())
            .ok_or_else(|| WorkflowError::Execution("workflow contains no steps".to_string()))?;

        // The loop counter acts as a runaway-guard — exceeding it is an error.
        for _ in 0..self.options.max_steps {
            // Look up the step definition by ID.
            let step = self.workflow.step_by_id(&current_step_id).ok_or_else(|| {
                WorkflowError::Execution(format!("unknown step `{current_step_id}`"))
            })?;

            info!("executing step `{}` with agent `{}`", step.id, step.agent);

            // Run the step (invoke the agent or produce a dry-run stub).
            let outcome = self.execute_step(step, &mut handoff_store)?;

            // If the step succeeded AND the workflow says this step needs review,
            // dispatch a review agent and check its decision.
            let reviewed = if outcome.success && self.workflow.is_review_required(step) {
                self.perform_review(step, &outcome.context, &outcome.result)?
            } else {
                false
            };

            // Record this step's result in the report.
            completed_steps.push(CompletedStep {
                step_id: step.id.clone(),
                agent: step.agent.clone(),
                reviewed,
                status: if self.options.dry_run {
                    StepExecutionStatus::DryRun
                } else if outcome.success {
                    StepExecutionStatus::Succeeded
                } else {
                    StepExecutionStatus::Failed
                },
            });

            // Pick the right transition block based on success/failure.
            let transition = if outcome.success {
                step.on_success.as_ref()
            } else {
                step.on_failure.as_ref()
            };

            // Decide where to go next (advance, halt, or end naturally).
            match self.resolve_transition(step, transition)? {
                TransitionResolution::Next(step_id) => {
                    // Update the loop variable and continue to the next iteration.
                    current_step_id = step_id;
                }
                TransitionResolution::Halt => {
                    // Deliberate stop — return now with a Halted status.
                    return Ok(ExecutionReport {
                        workflow_name: self.workflow.name.clone(),
                        dry_run: self.options.dry_run,
                        completed_steps,
                        final_status: if outcome.success {
                            RunStatus::Halted
                        } else {
                            RunStatus::Failed(format!("step `{}` failed", step.id))
                        },
                    });
                }
                TransitionResolution::End => {
                    // No next step declared and no halt — workflow ran to completion.
                    return Ok(ExecutionReport {
                        workflow_name: self.workflow.name.clone(),
                        dry_run: self.options.dry_run,
                        completed_steps,
                        final_status: RunStatus::Completed,
                    });
                }
            }
        }

        // We only reach here if the loop exhausted max_steps without halting.
        Err(WorkflowError::Execution(format!(
            "workflow exceeded max_steps limit ({})",
            self.options.max_steps
        )))
    }

    // ─── Private helpers ──────────────────────────────────────────────────────

    /// Run a single step — either invoke the real agent or produce a dry-run stub.
    fn execute_step(
        &self,
        step: &Step,
        handoff_store: &mut HandoffStore,
    ) -> Result<StepOutcome, WorkflowError> {
        // Read the handoff file *before* running the step so the agent sees
        // everything the previous agents wrote.
        let handoff_markdown = handoff_store.read()?;
        let handoff_path = handoff_store.path().to_path_buf();

        // Bundle everything the agent executor needs into one struct.
        let context = ExecutionContext {
            current_step_id: step.id.clone(),
            current_agent: step.agent.clone(),
            workspace_root: self.resolve_workspace(step.agent.as_str()),
            handoff_path,
            handoff_markdown,
        };

        // Dry-run: fabricate a success result without invoking anything real.
        if self.options.dry_run {
            let dry_result = StepAgentResult {
                status: AgentRunStatus::Success,
                summary: format!("dry run for step `{}`", step.id),
                current_state: Some("No work executed".to_string()),
                open_questions: Vec::new(),
                next_actions: vec!["Invoke the real agent executor".to_string()],
            };

            // Still write to the handoff file so downstream steps have context
            // (even in dry-run mode the state file is populated).
            handoff_store.record_step(step, &context, &dry_result)?;

            return Ok(StepOutcome {
                success: true,
                result: dry_result,
                context,
            });
        }

        // Real execution: call into agent.rs which spawns the subprocess.
        let result = self
            .agent_executor
            .run_step(&self.workflow, step, &context)?;

        // Run shell commands (cargo fmt, cargo test, etc.) only on success.
        // No point formatting code that the agent says is broken.
        if result.status == AgentRunStatus::Success {
            self.run_shell_commands(step, &context.workspace_root)?;
        }

        // Write the agent's structured result into the handoff file.
        handoff_store.record_step(step, &context, &result)?;

        Ok(StepOutcome {
            success: result.status == AgentRunStatus::Success,
            result,
            context,
        })
    }

    /// Dispatch a review agent to approve or reject the step result.
    ///
    /// Returns `Ok(true)` when the gate approves, or a `WorkflowError::StepFailed`
    /// with the blocking issues when it rejects.
    fn perform_review(
        &self,
        step: &Step,
        context: &ExecutionContext,
        step_result: &StepAgentResult,
    ) -> Result<bool, WorkflowError> {
        // The step must have named a gate — validation should have caught this,
        // but we guard against it here too.
        let gate_id = step.review.gate.as_deref().ok_or_else(|| {
            WorkflowError::Execution(format!(
                "step `{}` requires review but no gate was configured",
                step.id
            ))
        })?;

        let gate = self
            .workflow
            .review_gate_by_id(gate_id)
            .ok_or_else(|| WorkflowError::Execution(format!("unknown review gate `{gate_id}`")))?;

        // The reviewer is the *other* agent — the one that didn't run this step.
        let reviewer_agent = self.select_reviewer(step)?;

        // Dry-run: auto-approve so the workflow can be tested end-to-end.
        if self.options.dry_run {
            info!(
                "dry run review gate `{}` for step `{}` via agent `{}`",
                gate.id, step.id, reviewer_agent
            );
            return Ok(true);
        }

        // Real review: call the agent executor's review path.
        let decision = self.agent_executor.run_review(
            &self.workflow,
            gate,
            step,
            reviewer_agent.as_str(),
            context,
            step_result,
        )?;

        if decision.approved {
            Ok(true)
        } else {
            // Rejected — surface the blocking issues as a WorkflowError so the
            // runner loop handles failure the same way for both steps and reviews.
            Err(WorkflowError::StepFailed {
                step_id: step.id.clone(),
                message: if decision.blocking_issues.is_empty() {
                    format!("review gate `{}` rejected the step", gate.id)
                } else {
                    format!(
                        "review gate `{}` rejected the step: {}",
                        gate.id,
                        decision.blocking_issues.join("; ")
                    )
                },
            })
        }
    }

    /// Choose which agent should review the given step.
    ///
    /// Priority: prefer the agent named in `on_success.handoff_to` if it's
    /// different from the step's own agent.  Fall back to any other declared agent.
    fn select_reviewer(&self, step: &Step) -> Result<String, WorkflowError> {
        if let Some(transition) = step.on_success.as_ref() {
            if let Some(agent) = transition.handoff_to.as_ref() {
                // Only use handoff_to as the reviewer if it's a *different* agent.
                if agent != &step.agent {
                    return Ok(agent.clone());
                }
            }
        }

        // Fallback: pick any agent in the workflow that isn't this step's agent.
        self.workflow
            .agents
            .keys()
            .find(|agent| *agent != &step.agent)
            .cloned()
            .ok_or_else(|| {
                WorkflowError::Execution(format!(
                    "step `{}` requires a reviewer but no alternate agent exists",
                    step.id
                ))
            })
    }

    /// Run the `step.run` shell commands in the given workspace directory.
    ///
    /// Uses `sh -lc <command>` so shell builtins, aliases, and login-profile
    /// PATH additions are all available (e.g. cargo is found even if it's in ~/.cargo/bin).
    fn run_shell_commands(&self, step: &Step, workspace: &Path) -> Result<(), WorkflowError> {
        for command in &step.run {
            info!("running shell command for step `{}`: {}", step.id, command);

            let output = Command::new("sh")
                .arg("-lc")       // -l = login shell (sources profile), -c = run the command
                .arg(command)
                .current_dir(workspace)
                .output()
                .map_err(|error| WorkflowError::StepFailed {
                    step_id: step.id.clone(),
                    message: format!("failed to start `{command}`: {error}"),
                })?;

            // A non-zero exit code means the command failed (e.g. tests failed).
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                return Err(WorkflowError::StepFailed {
                    step_id: step.id.clone(),
                    message: format!(
                        "shell command `{command}` failed with status {}.\nstdout:\n{}\nstderr:\n{}",
                        output.status,
                        stdout.trim(),
                        stderr.trim()
                    ),
                });
            }
        }

        Ok(())
    }

    /// Interpret the transition block to decide what the runner does next.
    fn resolve_transition(
        &self,
        step: &Step,
        transition: Option<&StepTransition>,
    ) -> Result<TransitionResolution, WorkflowError> {
        // No transition at all means "workflow ends naturally after this step".
        let Some(transition) = transition else {
            return Ok(TransitionResolution::End);
        };

        match transition.route {
            StepRoute::Halt => Ok(TransitionResolution::Halt),

            // Both Continue and Retry advance to `next_step` when one is provided.
            // Retry falls back to re-running the *current* step when next_step is absent.
            StepRoute::Retry | StepRoute::Continue => {
                if let Some(next_step) = transition.next_step.as_ref() {
                    return Ok(TransitionResolution::Next(next_step.clone()));
                }

                if matches!(transition.route, StepRoute::Retry) {
                    // No explicit next_step on a retry → re-run the same step.
                    return Ok(TransitionResolution::Next(step.id.clone()));
                }

                // Continue with no next_step → graceful end of workflow.
                warn!(
                    "step `{}` has route {:?} but no next_step; ending workflow",
                    step.id, transition.route
                );
                Ok(TransitionResolution::End)
            }
        }
    }

    /// Resolve the workspace directory for a named agent.
    ///
    /// Priority: agent-specific workspace → workflow defaults.working_directory → workspace_root.
    fn resolve_workspace(&self, agent_name: &str) -> PathBuf {
        // `defaults.working_directory` is relative to the workspace root.
        let base = self
            .workflow
            .defaults
            .working_directory
            .as_deref()
            .map(|dir| self.workspace_root.join(dir))
            .unwrap_or_else(|| self.workspace_root.clone());

        let Some(agent) = self.workflow.agents.get(agent_name) else {
            return base; // unknown agent — use the default base
        };

        match agent.workspace.as_deref() {
            Some(workspace) => self.workspace_root.join(workspace),
            None => base,
        }
    }
}

// ─── Private internal types ───────────────────────────────────────────────────

/// Intermediate result from `execute_step`, before we record it in the report.
struct StepOutcome {
    success: bool,
    result: StepAgentResult,
    context: ExecutionContext,
}

/// What the runner should do after a transition is resolved.
enum TransitionResolution {
    /// Advance to a specific step ID.
    Next(String),
    /// Stop the workflow cleanly (route: halt).
    Halt,
    /// No more steps — workflow completed naturally.
    End,
}

// ─── Prompt helpers (pub(crate) — used by agent.rs) ──────────────────────────

/// Build the final prompt string for a step by expanding snippet triggers and
/// appending named snippets and the current handoff markdown.
///
/// Two injection mechanisms:
///  1. Trigger strings (e.g. ";;arch") embedded inline in the prompt text get
///     replaced by the snippet content wherever they appear.
///  2. Named snippets listed in `step.snippets` are appended at the end if
///     their content isn't already present (avoids duplicate injection).
pub(crate) fn expand_step_prompt(
    workflow: &Workflow,
    step: &Step,
    handoff_markdown: &str,
) -> String {
    let mut prompt = step.prompt.clone().unwrap_or_default();

    // Pass 1: replace inline trigger strings with snippet content.
    for snippet in workflow.snippets.values() {
        if prompt.contains(&snippet.trigger) {
            prompt = prompt.replace(&snippet.trigger, &snippet.content);
        }
    }

    // Pass 2: append snippets referenced by name in step.snippets.
    for snippet_name in &step.snippets {
        if let Some(snippet) = workflow.snippets.get(snippet_name) {
            // Skip if the snippet content is already present (e.g. was injected via trigger above).
            if !prompt.contains(&snippet.content) {
                if !prompt.is_empty() {
                    prompt.push_str("\n\n");
                }
                prompt.push_str(&format!(
                    "Snippet `{}` ({})\n{}",
                    snippet_name, snippet.trigger, snippet.content
                ));
            }
        }
    }

    // Append the handoff context last so the agent sees the most recent state.
    if !handoff_markdown.trim().is_empty() {
        if !prompt.is_empty() {
            prompt.push_str("\n\n");
        }
        prompt.push_str("Current handoff context:\n");
        prompt.push_str(handoff_markdown);
    }

    prompt
}

/// Look up the ConductorAgent (provider enum) for a given step.
pub(crate) fn provider_for_step<'a>(
    workflow: &'a Workflow,
    step: &Step,
) -> Result<&'a ConductorAgent, WorkflowError> {
    workflow
        .agents
        .get(step.agent.as_str())
        .map(|agent| &agent.provider)
        .ok_or_else(|| WorkflowError::Execution(format!("unknown agent `{}`", step.agent)))
}

/// Convert `HandoffMode` to a human-readable string for error messages.
pub(crate) fn handoff_mode_name(mode: &HandoffMode) -> &'static str {
    match mode {
        HandoffMode::Append => "append",
        HandoffMode::Replace => "replace",
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::fs;
    use std::path::Path;

    use crate::workflow::{ReviewGate, load_workflow};

    use super::*;

    // A mock executor that replays pre-canned results from a queue.
    // `RefCell` lets us mutate the queues through a shared reference,
    // which is required because `AgentExecutor` methods take `&self`.
    struct MockAgentExecutor {
        steps: RefCell<VecDeque<Result<StepAgentResult, WorkflowError>>>,
        reviews: RefCell<VecDeque<Result<ReviewDecision, WorkflowError>>>,
    }

    impl MockAgentExecutor {
        fn new(
            steps: Vec<Result<StepAgentResult, WorkflowError>>,
            reviews: Vec<Result<ReviewDecision, WorkflowError>>,
        ) -> Self {
            Self {
                steps: RefCell::new(VecDeque::from(steps)),
                reviews: RefCell::new(VecDeque::from(reviews)),
            }
        }
    }

    impl AgentExecutor for MockAgentExecutor {
        fn run_step(
            &self,
            _workflow: &Workflow,
            _step: &Step,
            _context: &ExecutionContext,
        ) -> Result<StepAgentResult, WorkflowError> {
            self.steps
                .borrow_mut()
                .pop_front()
                .expect("missing step result")
        }

        fn run_review(
            &self,
            _workflow: &Workflow,
            _gate: &ReviewGate,
            _step: &Step,
            _reviewer_agent: &str,
            _context: &ExecutionContext,
            _step_result: &StepAgentResult,
        ) -> Result<ReviewDecision, WorkflowError> {
            self.reviews
                .borrow_mut()
                .pop_front()
                .expect("missing review result")
        }
    }

    #[test]
    fn expands_trigger_and_named_snippets() {
        let workflow =
            load_workflow(Path::new("/home/tom/conductor/workflow.yaml")).expect("workflow");
        let step = workflow.step_by_id("review_design").expect("step");
        let prompt = expand_step_prompt(&workflow, step, "# Prior handoff");

        assert!(prompt.contains("Review the proposed design"));
        assert!(prompt.contains("Review this change for correctness"));
        assert!(prompt.contains("# Prior handoff"));
    }

    #[test]
    fn executes_reviewed_workflow_path() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let workflow_path = tempdir.path().join("workflow.yaml");
        fs::write(
            &workflow_path,
            r#"
version: "1"
name: "runner-test"
handoff:
  path: "handover.md"
agents:
  claude:
    provider: "claude_code"
  codex:
    provider: "open_ai_codex"
steps:
  - id: "design"
    title: "Design"
    agent: "claude"
    on_success:
      handoff_to: "codex"
      next_step: "review"
      route: "continue"
  - id: "review"
    title: "Review"
    agent: "codex"
    review:
      required: true
      gate: "quality_gate"
    on_success:
      next_step: "ship"
      route: "continue"
  - id: "ship"
    title: "Ship"
    agent: "claude"
    on_success:
      route: "halt"
review_gates:
  - id: "quality_gate"
    name: "Quality Gate"
"#,
        )
        .expect("write workflow");
        let workflow = load_workflow(&workflow_path).expect("workflow");
        let runner = Runner::with_agent_executor(
            workflow,
            &workflow_path,
            RunnerOptions {
                dry_run: false,
                max_steps: 8,
            },
            Box::new(MockAgentExecutor::new(
                vec![
                    Ok(StepAgentResult {
                        status: AgentRunStatus::Success,
                        summary: "design done".to_string(),
                        current_state: Some("designed".to_string()),
                        open_questions: Vec::new(),
                        next_actions: vec!["review".to_string()],
                    }),
                    Ok(StepAgentResult {
                        status: AgentRunStatus::Success,
                        summary: "review step done".to_string(),
                        current_state: Some("reviewed".to_string()),
                        open_questions: Vec::new(),
                        next_actions: vec!["ship".to_string()],
                    }),
                    Ok(StepAgentResult {
                        status: AgentRunStatus::Success,
                        summary: "shipped".to_string(),
                        current_state: Some("released".to_string()),
                        open_questions: Vec::new(),
                        next_actions: Vec::new(),
                    }),
                ],
                vec![Ok(ReviewDecision {
                    approved: true,
                    summary: "approved".to_string(),
                    blocking_issues: Vec::new(),
                })],
            )),
        );

        let report = runner.run().expect("runner should succeed");
        assert_eq!(report.completed_steps.len(), 3);
        assert!(matches!(report.final_status, RunStatus::Halted));
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
// • `Box<dyn Trait>` (trait objects) — lets you store any type that implements
//   the trait without knowing the concrete type at compile time.  The `dyn`
//   keyword signals "dynamic dispatch" (a vtable is used at runtime).
// • `#[cfg(test)]` — the annotated code is only compiled when running `cargo test`,
//   keeping the production binary clean of test infrastructure.
// • `RefCell<T>` — allows mutating data through a shared `&self` reference at
//   the cost of a runtime borrow check.  Useful for test mocks where trait
//   methods take `&self` but we need interior mutability.
// • State machine pattern: the `current_step_id` variable + `TransitionResolution`
//   enum drives the loop — each iteration computes where to go next without
//   recursion.
// • `matches!(expr, Pattern)` — a concise bool check for enum variants;
//   equivalent to `if let Pattern = expr { true } else { false }`.
