// =============================================================================
// workflow/validate.rs — Semantic validation for a loaded Workflow
//
// Serde ensures the YAML is structurally correct (right types, required fields
// present).  This module checks the *meaning* of the values:
//
//  • No duplicate step IDs
//  • Every step references an agent that was declared
//  • Every snippet reference in a step exists in workflow.snippets
//  • Every review gate reference in a step exists in workflow.review_gates
//  • Steps that require review must declare a gate
//  • on_success / on_failure next_step and handoff_to values point at known IDs
//
// All issues are collected into a `Vec<String>` before returning a single
// `WorkflowError::Validation` — so the user sees all problems at once instead
// of having to fix one and re-run to find the next.
// =============================================================================

// `HashSet` gives O(1) membership checks.
// We build sets of known agent/step/gate IDs and test references against them.
use std::collections::HashSet;

use crate::error::WorkflowError;

use super::Workflow;

/// Validate the semantic consistency of a workflow.
///
/// Returns `Ok(())` when all checks pass, or `Err(WorkflowError::Validation)`
/// with a newline-separated list of all detected problems.
pub fn validate_workflow(workflow: &Workflow) -> Result<(), WorkflowError> {
    // Collect all issues rather than short-circuiting at the first one.
    let mut issues = Vec::new();

    // ── Basic field checks ────────────────────────────────────────────────────

    if workflow.version.trim().is_empty() {
        issues.push("`version` must not be empty".to_string());
    }

    if workflow.name.trim().is_empty() {
        issues.push("`name` must not be empty".to_string());
    }

    if workflow.agents.is_empty() {
        issues.push("at least one agent must be defined".to_string());
    }

    if workflow.steps.is_empty() {
        issues.push("at least one step must be defined".to_string());
    }

    // ── Build lookup sets from declared IDs ───────────────────────────────────

    // `keys()` iterates over the agent names (Strings); `map(String::as_str)`
    // converts each &String to a &str so HashSet<&str> works without cloning.
    let agent_ids: HashSet<_> = workflow.agents.keys().map(String::as_str).collect();

    let review_gate_ids: HashSet<_> = workflow
        .review_gates
        .iter()
        .map(|gate| gate.id.as_str())
        .collect();

    // ── Per-step checks ───────────────────────────────────────────────────────

    // Track seen step IDs to catch duplicates in a single pass.
    let mut step_ids = HashSet::new();

    for step in &workflow.steps {
        // `insert` returns false when the value was already present — that's a duplicate.
        if !step_ids.insert(step.id.as_str()) {
            issues.push(format!("duplicate step id `{}`", step.id));
        }

        if step.title.trim().is_empty() {
            issues.push(format!("step `{}` must have a non-empty title", step.id));
        }

        // Ensure the agent name this step uses was declared in `workflow.agents`.
        if !agent_ids.contains(step.agent.as_str()) {
            issues.push(format!(
                "step `{}` references unknown agent `{}`",
                step.id, step.agent
            ));
        }

        // Check every snippet name listed in `step.snippets`.
        for snippet_name in &step.snippets {
            if !workflow.snippets.contains_key(snippet_name) {
                issues.push(format!(
                    "step `{}` references unknown snippet `{}`",
                    step.id, snippet_name
                ));
            }
        }

        // Validate the review gate reference if one is declared.
        if let Some(gate) = step.review.gate.as_deref() {
            if !review_gate_ids.contains(gate) {
                issues.push(format!(
                    "step `{}` references unknown review gate `{}`",
                    step.id, gate
                ));
            }
        }

        // A step that says review is required must actually name a gate;
        // otherwise the runner would try to find one and fail at runtime.
        if workflow.is_review_required(step) && step.review.gate.is_none() {
            issues.push(format!(
                "step `{}` requires review but does not declare a `review.gate`",
                step.id
            ));
        }
    }

    // ── Transition reference checks ───────────────────────────────────────────

    // Build the full set of known step IDs *after* we've scanned all steps.
    // (step_ids above was mutated during the loop; re-collect from the workflow.)
    let known_steps: HashSet<_> = workflow.steps.iter().map(|step| step.id.as_str()).collect();

    for step in &workflow.steps {
        // Check both transitions in one loop using an array of (label, Option<&Transition>).
        // This avoids writing the same validation logic twice.
        for (transition_name, transition) in [
            ("on_success", step.on_success.as_ref()),
            ("on_failure", step.on_failure.as_ref()),
        ] {
            // `if let Some(transition)` destructures the Option — skip None transitions.
            if let Some(transition) = transition {
                // `next_step` must point to a step that was declared.
                if let Some(next_step) = transition.next_step.as_deref() {
                    if !known_steps.contains(next_step) {
                        issues.push(format!(
                            "step `{}` {} references unknown next step `{}`",
                            step.id, transition_name, next_step
                        ));
                    }
                }

                // `handoff_to` must point to an agent that was declared.
                if let Some(agent) = transition.handoff_to.as_deref() {
                    if !agent_ids.contains(agent) {
                        issues.push(format!(
                            "step `{}` {} references unknown handoff agent `{}`",
                            step.id, transition_name, agent
                        ));
                    }
                }
            }
        }
    }

    // ── Report all issues at once ─────────────────────────────────────────────

    if !issues.is_empty() {
        // Join all issue strings with newlines for a readable multi-line error.
        return Err(WorkflowError::Validation(issues.join("\n")));
    }

    Ok(())
}

// =============================================================================
// Learning Notes
// =============================================================================
// • Collect-then-report pattern: gather all issues in a Vec before returning
//   an error so users don't have to fix-run-fix-run-fix-run.
// • `HashSet::insert` returns a bool: true = was new, false = already present.
//   This is an idiomatic duplicate-detection pattern in Rust.
// • `as_deref()` on `Option<String>` gives `Option<&str>` — useful when you
//   need to pass a borrowed reference without cloning the String.
// • Iterating over an array of tuples like `[("on_success", ...), ("on_failure", ...)]`
//   is a clean way to de-duplicate validation logic that applies to multiple fields.
