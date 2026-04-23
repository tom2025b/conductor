// =============================================================================
// workflow/model.rs — Data model for workflow YAML files
//
// Each Rust struct here maps 1-to-1 with a YAML key in workflow.yaml.
// `serde` (with the "derive" feature) generates serialization/deserialization
// code at compile time.  `#[derive(Deserialize)]` lets serde_yaml convert a
// YAML string into these structs; `#[derive(Serialize)]` goes the other way.
//
// Key things to understand in this file:
//  • `#[serde(default)]` — missing YAML keys get a Rust default value
//  • `#[serde(rename_all = "snake_case")]` — maps Rust enum variants to YAML strings
//  • `IndexMap` — like HashMap but preserves insertion order (important for YAML)
//  • Helper functions like `default_true()` supply non-zero defaults for serde
// =============================================================================

// `IndexMap` is from the `indexmap` crate.  It behaves like `HashMap` but
// remembers the order keys were inserted — critical when we iterate over
// steps or agents and want them in the order the user wrote them in YAML.
use indexmap::IndexMap;

// `Deserialize` = "can be built from a data format like YAML or JSON"
// `Serialize`   = "can be converted back to a data format"
use serde::{Deserialize, Serialize};

// Serde can't express "this field defaults to `true`" directly because Rust's
// `Default` for bool is `false`.  The workaround: write a tiny function that
// returns `true` and point serde at it with `#[serde(default = "default_true")]`.
fn default_true() -> bool {
    true
}

// ─── Top-level workflow ───────────────────────────────────────────────────────

/// The root struct — mirrors the entire workflow YAML document.
///
/// Every public field corresponds to a top-level key.
/// Fields annotated with `#[serde(default)]` are optional in the YAML file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    /// Schema version string (currently always "1").
    pub version: String,

    /// Human-readable name for this workflow.
    pub name: String,

    /// Optional free-form description shown in --summary output.
    #[serde(default)]
    pub description: Option<String>,

    /// Global defaults that apply to all steps unless overridden.
    #[serde(default)]
    pub defaults: WorkflowDefaults,

    /// Configuration for the handoff Markdown file passed between agents.
    #[serde(default)]
    pub handoff: HandoffConfig,

    /// Named reusable text snippets that can be injected into step prompts.
    /// `IndexMap` preserves the order they appear in the YAML.
    #[serde(default)]
    pub snippets: IndexMap<String, Snippet>,

    /// Named agent definitions (keys are used in step.agent fields).
    pub agents: IndexMap<String, AgentConfig>,

    /// Ordered list of steps in the workflow.
    pub steps: Vec<Step>,

    /// Review gates referenced by steps that require approval.
    #[serde(default)]
    pub review_gates: Vec<ReviewGate>,
}

impl Workflow {
    /// Find a step by its id field, returning a reference or None.
    pub fn step_by_id(&self, step_id: &str) -> Option<&Step> {
        // `iter().find()` scans linearly — fine for typical workflow sizes (< 50 steps).
        self.steps.iter().find(|step| step.id == step_id)
    }

    /// Find a review gate by its id field.
    pub fn review_gate_by_id(&self, gate_id: &str) -> Option<&ReviewGate> {
        self.review_gates.iter().find(|gate| gate.id == gate_id)
    }

    /// Returns true if this step requires a review gate to pass before continuing.
    ///
    /// Step-level `review.required` overrides the workflow-level default.
    /// `Option::unwrap_or` supplies the fallback when the step didn't set it.
    pub fn is_review_required(&self, step: &Step) -> bool {
        step.review
            .required
            .unwrap_or(self.defaults.review_required)
    }
}

// ─── Defaults ─────────────────────────────────────────────────────────────────

/// Workflow-wide defaults; any step can override these fields individually.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowDefaults {
    /// When true, each step automatically writes a handoff entry after running.
    #[serde(default = "default_true")] // "true" unless the YAML sets it false
    pub auto_handoff: bool,

    /// When true, every step requires a review gate (unless the step opts out).
    #[serde(default)] // defaults to false (Rust's bool default)
    pub review_required: bool,

    /// Directory all agents use as their working root (relative to the workflow file).
    #[serde(default)]
    pub working_directory: Option<String>,
}

// ─── Handoff ──────────────────────────────────────────────────────────────────

/// Controls the Markdown file that carries context from one agent to the next.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffConfig {
    /// File path (relative to the workflow file) where handoff notes are written.
    #[serde(default = "default_handoff_path")]
    pub path: String,

    /// Whether new entries are appended to the file or replace it entirely.
    #[serde(default)]
    pub mode: HandoffMode,

    /// Section headings that every handoff entry must include.
    #[serde(default)]
    pub required_sections: Vec<String>,
}

// `Default` must be implemented manually because `default_handoff_path()` is not
// a const — we can't use the derive macro here.
impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            path: default_handoff_path(),
            mode: HandoffMode::default(),
            required_sections: Vec::new(),
        }
    }
}

fn default_handoff_path() -> String {
    "handover.md".to_string()
}

/// How the handoff file is updated after each step.
///
/// `#[serde(rename_all = "snake_case")]` maps `Append` → `"append"` and
/// `Replace` → `"replace"` in YAML, matching what users type.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffMode {
    #[default] // `Append` is used when the YAML field is omitted
    Append,
    Replace,
}

// ─── Snippets ─────────────────────────────────────────────────────────────────

/// A named block of text that can be injected into a step's prompt.
///
/// Snippets reduce repetition: common instructions (architecture briefs,
/// review requests) are defined once and referenced by name or trigger string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snippet {
    /// Short string the agent can type inline to trigger injection (e.g. ";;arch").
    pub trigger: String,

    /// The actual text that replaces the trigger or gets appended to the prompt.
    pub content: String,

    /// Optional human-readable description of what this snippet does.
    #[serde(default)]
    pub description: Option<String>,
}

// ─── Agents ───────────────────────────────────────────────────────────────────

/// Configuration for a single agent (Claude Code or OpenAI Codex).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Which AI provider backs this agent.
    pub provider: ConductorAgent,

    /// Optional model override (e.g. "sonnet", "gpt-5.4").
    #[serde(default)]
    pub model: Option<String>,

    /// Optional profile name passed to the underlying CLI.
    #[serde(default)]
    pub profile: Option<String>,

    /// Optional per-agent working directory override.
    #[serde(default)]
    pub workspace: Option<String>,

    /// Extra environment variables injected into the agent subprocess.
    /// `IndexMap<String, String>` preserves the order from the YAML.
    #[serde(default)]
    pub env: IndexMap<String, String>,
}

/// The two agent provider backends Conductor knows how to invoke.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")] // "claude_code" / "open_ai_codex" in YAML
pub enum ConductorAgent {
    ClaudeCode,
    OpenAiCodex,
}

// ─── Steps ────────────────────────────────────────────────────────────────────

/// One unit of work in the workflow — assigned to a single agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Step {
    /// Unique identifier used to reference this step in transitions.
    pub id: String,

    /// Human-readable label shown in progress output.
    pub title: String,

    /// Name of the agent (must match a key in `workflow.agents`) that runs this step.
    pub agent: String,

    /// The prompt text sent to the agent.  May contain snippet triggers.
    #[serde(default)]
    pub prompt: Option<String>,

    /// Shell commands Conductor runs in the workspace after a successful agent run.
    /// Useful for formatters, linters, and test suites.
    #[serde(default)]
    pub run: Vec<String>,

    /// Named snippets appended to the prompt before the agent is invoked.
    #[serde(default)]
    pub snippets: Vec<String>,

    /// Whether this step requires review and which gate it uses.
    #[serde(default)]
    pub review: ReviewRequirement,

    /// Transition to take when the agent reports success.
    #[serde(default)]
    pub on_success: Option<StepTransition>,

    /// Transition to take when the agent reports failure.
    #[serde(default)]
    pub on_failure: Option<StepTransition>,
}

/// Specifies whether and how a step must be reviewed before the workflow advances.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReviewRequirement {
    /// ID of the review gate that must approve this step.
    #[serde(default)]
    pub gate: Option<String>,

    /// Explicit override for the workflow-level `defaults.review_required` flag.
    /// `None` means "use the default"; `Some(false)` opts this step out.
    #[serde(default)]
    pub required: Option<bool>,
}

/// Describes what happens after a step succeeds or fails.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepTransition {
    /// Agent to pass context to before running the next step.
    #[serde(default)]
    pub handoff_to: Option<String>,

    /// ID of the step to execute next.
    #[serde(default)]
    pub next_step: Option<String>,

    /// High-level routing instruction.
    #[serde(default)]
    pub route: StepRoute,
}

/// How the workflow moves after a step finishes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepRoute {
    /// Advance to `next_step` (or end if none is set).
    #[default]
    Continue,

    /// Run the same step again (useful for plan-then-retry loops).
    Retry,

    /// Stop the workflow immediately and report Halted status.
    Halt,
}

// ─── Review gates ─────────────────────────────────────────────────────────────

/// A named checkpoint that an agent must approve before the workflow continues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewGate {
    /// Unique identifier used in `step.review.gate`.
    pub id: String,

    /// Human-readable name shown in review prompts.
    pub name: String,

    /// How many approvers are needed (currently informational; always checked by one agent).
    #[serde(default)]
    pub required_approvers: u8,

    /// Extra instructions injected into the review prompt.
    #[serde(default)]
    pub instructions: Option<String>,
}

// =============================================================================
// Learning Notes
// =============================================================================
// • `#[derive(Serialize, Deserialize)]` — serde generates all conversion code
//   at compile time; you don't write any parsing logic yourself.
// • `#[serde(default)]` on a field — if the YAML key is absent, serde calls
//   Default::default() (or your custom function) instead of erroring.
// • `IndexMap` vs `HashMap` — IndexMap preserves insertion order; important
//   here because step ordering in the workflow must match the YAML order.
// • `Option<T>` fields — distinguish "user set this to X" from "user didn't
//   mention it"; lets higher-level code apply fallback logic cleanly.
// • `#[serde(rename_all = "snake_case")]` on an enum — maps Rust PascalCase
//   variant names to lowercase_with_underscores strings in YAML/JSON.
