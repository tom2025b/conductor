# Conductor

A CLI orchestrator for multi-agent workflows between **Claude Code** and **OpenAI Codex**.

Conductor lets you define a `workflow.yaml` that specifies how two AI agents hand work off to each other — plan → review → implement → final review — with human-readable handoff notes, review gates, and validation shell commands at every step.

---

## What It Does

- **`conductor validate`** — parse and validate a `workflow.yaml` file, reporting all issues at once
- **`conductor run`** — execute the workflow step by step, invoking Claude Code and Codex as subprocesses
- **`conductor regen`** — inspect the current project directory and auto-generate a `workflow.yaml`
- **`codex regen`** — same as above but callable directly from inside a Codex session

Workflows run in **dry-run mode by default** (no real agent calls). Add `--live` to invoke the real CLIs.

---

## Install

```sh
# Build both binaries
cargo build --release

# Optional: put them on PATH
cp target/release/conductor ~/.local/bin/
cp target/release/codex     ~/.local/bin/   # shim — intercepts 'codex regen'
```

---

## Quick Start

### Auto-generate a workflow

```sh
cd your-project/
conductor regen
# or from inside a Codex session:
codex regen
```

Conductor inspects your project (Cargo.toml, package.json, pyproject.toml) and writes a `workflow.yaml` tailored to it.

### Validate a workflow

```sh
conductor validate
conductor validate --workflow path/to/workflow.yaml --summary
```

### Run dry (default — no real agent calls)

```sh
conductor run
```

### Run live (invokes Claude Code and Codex)

```sh
conductor run --live
conductor run --live --max-steps 16
```

---

## Workflow File Format

```yaml
version: "1"
name: "my-project"
description: "Optional description"

defaults:
  auto_handoff: true     # each step automatically writes a handoff entry
  review_required: true  # every step requires a gate by default
  working_directory: "."

handoff:
  path: "handover.md"    # agents read/write this file for context
  mode: "append"         # or "replace"
  required_sections:
    - "Current State"
    - "Open Questions"
    - "Next Actions"

snippets:
  my_brief:
    trigger: ";;brief"                     # inline trigger the agent can use
    content: "Keep changes small and..."   # text injected at the trigger site

agents:
  claude:
    provider: "claude_code"
    model: "sonnet"
    workspace: "."
  codex:
    provider: "open_ai_codex"
    model: "gpt-5.4"
    workspace: "."

steps:
  - id: "design"
    title: "Design the approach"
    agent: "claude"
    prompt: "Draft the implementation plan. Use ;;brief."
    review:
      required: false
    on_success:
      handoff_to: "codex"
      next_step: "review"
      route: "continue"

  - id: "review"
    title: "Review the plan"
    agent: "codex"
    review:
      gate: "quality_gate"
      required: true
    on_success:
      route: "halt"
    on_failure:
      next_step: "design"
      route: "retry"

review_gates:
  - id: "quality_gate"
    name: "Quality Gate"
    required_approvers: 1
    instructions: "Check for correctness and regression risk."
```

---

## Step Routing

| `route`    | Behaviour |
|------------|-----------|
| `continue` | Advance to `next_step` (or end naturally if none) |
| `retry`    | Re-run the same step (or advance to `next_step` if set) |
| `halt`     | Stop the workflow immediately and report **Halted** |

---

## Review Gates

When a step has `review.required: true` and a named `review.gate`, Conductor:

1. Runs the step with the assigned agent.
2. Dispatches the *other* agent to review the result.
3. The reviewer returns `{ "approved": true/false, "blocking_issues": [...] }`.
4. If rejected, the step is treated as failed and `on_failure` routing applies.

---

## Environment Variables

| Variable | Default | Purpose |
|---|---|---|
| `CONDUCTOR_WORKFLOW` | `workflow.yaml` | Path to the workflow file |
| `CONDUCTOR_CLAUDE_BIN` | `claude` | Path to the Claude Code CLI binary |
| `CONDUCTOR_CODEX_BIN` | `codex` | Path to the Codex CLI binary |
| `CONDUCTOR_CLAUDE_PERMISSION_MODE` | `acceptEdits` | Claude filesystem permission mode |
| `CONDUCTOR_CODEX_SANDBOX` | `workspace-write` | Codex sandbox restriction level |
| `CONDUCTOR_CODEX_FULL_AUTO` | `1` (enabled) | Set to `0` or `false` to disable Codex full-auto mode |
| `CONDUCTOR_CODEX_PROFILE` | _(unset)_ | Codex profile name to pass via `--profile` |
| `CONDUCTOR_CODEX_HOME` | _(unset)_ | Override Codex's home/session directory |

---

## Project Structure

```
src/
  main.rs               — CLI entry point (parse, dispatch, print)
  lib.rs                — library root (re-exports all modules)
  cli.rs                — clap argument definitions
  error.rs              — WorkflowError enum (thiserror)
  regen.rs              — project analysis + workflow.yaml generation
  workflow/
    mod.rs              — public re-exports
    model.rs            — serde data model (mirrors the YAML structure)
    loader.rs           — read + parse + validate a workflow file
    validate.rs         — semantic validation (references, required fields)
  runner/
    mod.rs              — execution engine (step-by-step state machine)
    agent.rs            — Claude Code and Codex subprocess adapters
    handoff.rs          — handoff Markdown file management
  bin/
    codex.rs            — `codex regen` shim binary
tests/
  workflow_loader.rs    — integration tests for loading real workflow files
```

---

## How the Execution Engine Works

The runner drives a simple state machine:

```
[start] → execute step → review? → resolve transition → [next step ID]
                                                       → [halt]
                                                       → [end]
```

Each step calls into either `ClaudeAdapter` or `CodexAdapter`, which:

1. Writes a JSON Schema to a temp file.
2. Spawns the CLI binary with `--json-schema` and the prompt.
3. Reads the structured JSON output back.
4. Deserializes it into `StepAgentResult` or `ReviewDecision`.

The handoff file (`handover.md`) is read at the start of each step and written at the end, giving every agent full context about what happened before it.

---

## Running Tests

```sh
cargo test
```

The test suite uses `tempfile` for isolated filesystem fixtures and a `MockAgentExecutor` for runner logic tests — no real agent calls are made.

---

## License

MIT — Thomas Lane
