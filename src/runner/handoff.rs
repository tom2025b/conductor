// =============================================================================
// runner/handoff.rs — Manage the handoff Markdown file between agents
//
// When one agent finishes a step, Conductor writes a structured Markdown
// entry to the handoff file.  The next agent reads this file at the start of
// its step so it knows what the previous agent did, what questions remain open,
// and what it should do next.
//
// This module provides `HandoffStore` — a small struct that wraps the file
// path, the write mode, and the required-sections list.  It handles:
//   • `read`         — return the current file contents (or "" if not yet created)
//   • `record_step`  — format and write (or append) a new entry after a step
// =============================================================================

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::WorkflowError;
use crate::workflow::{HandoffMode, Step};

use super::{ExecutionContext, StepAgentResult, handoff_mode_name};

/// Wraps the handoff file state and knows how to format entries.
pub struct HandoffStore {
    /// Absolute path to the handoff Markdown file.
    path: PathBuf,

    /// Whether new entries are appended to the end or replace the whole file.
    mode: HandoffMode,

    /// Section headings that must appear in every entry (e.g. "Current State").
    required_sections: Vec<String>,
}

impl HandoffStore {
    pub fn new(path: PathBuf, mode: HandoffMode, required_sections: Vec<String>) -> Self {
        Self {
            path,
            mode,
            required_sections,
        }
    }

    /// Borrow the file path (for embedding in prompts and error messages).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read the current handoff file content.
    ///
    /// Returns an empty string if the file doesn't exist yet — that's normal
    /// on the first step, before any agent has written a handoff.
    pub fn read(&self) -> Result<String, WorkflowError> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => Ok(contents),
            // `ErrorKind::NotFound` is expected on the first step; treat it as "".
            // Any other IO error (permission denied, broken filesystem) is a real problem.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(WorkflowError::Execution(format!(
                "failed to read handoff file `{}`: {}",
                self.path.display(),
                error
            ))),
        }
    }

    /// Format and persist a handoff entry for the completed step.
    ///
    /// In `Append` mode the entry is concatenated after any existing content.
    /// In `Replace` mode the file is overwritten with just the new entry.
    pub fn record_step(
        &mut self,
        step: &Step,
        context: &ExecutionContext,
        result: &StepAgentResult,
    ) -> Result<(), WorkflowError> {
        // Ensure the parent directory exists; `create_dir_all` is a no-op if it does.
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                WorkflowError::Execution(format!(
                    "failed to create handoff directory `{}`: {}",
                    parent.display(),
                    error
                ))
            })?;
        }

        // Build the Markdown entry for this step.
        let entry = self.format_entry(step, context, result);

        // Decide whether to concatenate or overwrite.
        let contents = match self.mode {
            HandoffMode::Append => {
                let current = self.read()?;
                if current.trim().is_empty() {
                    // First entry — no separator needed.
                    entry
                } else {
                    // Separate successive entries with a blank line.
                    format!("{current}\n\n{entry}")
                }
            }
            // Replace: discard old content; only the latest step is kept.
            HandoffMode::Replace => entry,
        };

        // Write (create or overwrite) the file with the assembled content.
        fs::write(&self.path, contents).map_err(|error| {
            WorkflowError::Execution(format!(
                "failed to write handoff file `{}` in {} mode: {}",
                self.path.display(),
                handoff_mode_name(&self.mode), // "append" or "replace" for the error message
                error
            ))
        })
    }

    /// Build the Markdown text for one completed step.
    ///
    /// The output includes a level-2 header, metadata lines, and then a
    /// section for each heading in `required_sections`.  If no sections are
    /// required, all three standard sections are included by default.
    fn format_entry(
        &self,
        step: &Step,
        context: &ExecutionContext,
        result: &StepAgentResult,
    ) -> String {
        // `current_state` falls back to the summary when the agent didn't fill it in.
        let current_state = result
            .current_state
            .clone()
            .unwrap_or_else(|| result.summary.clone());

        // Convert the Vec<String> fields to "- item" Markdown lists.
        let open_questions = markdown_list(&result.open_questions);
        let next_actions = markdown_list(&result.next_actions);

        // Build the entry incrementally using a Vec of section strings.
        let mut sections = Vec::new();

        // Header and metadata lines come first.
        sections.push(format!("## Step `{}`: {}", step.id, step.title));
        sections.push(format!("- Agent: `{}`", context.current_agent));
        sections.push(format!(
            "- Workspace: `{}`",
            context.workspace_root.display()
        ));
        sections.push(format!("- Summary: {}", result.summary));

        // Append whichever required sections the workflow declared.
        for section in &self.required_sections {
            match section.as_str() {
                "Current State"   => sections.push(format!("### Current State\n{current_state}")),
                "Open Questions"  => sections.push(format!("### Open Questions\n{open_questions}")),
                "Next Actions"    => sections.push(format!("### Next Actions\n{next_actions}")),
                // Unknown section names become stub headings so the file is always valid.
                other             => sections.push(format!("### {other}\nPending")),
            }
        }

        // If the workflow didn't declare any required sections, include all three
        // standard ones so the file is always useful.
        if self.required_sections.is_empty() {
            sections.push(format!("### Current State\n{current_state}"));
            sections.push(format!("### Open Questions\n{open_questions}"));
            sections.push(format!("### Next Actions\n{next_actions}"));
        }

        // Join sections with blank lines for readable Markdown.
        sections.join("\n\n")
    }
}

/// Convert a slice of strings into a Markdown bullet list.
/// Returns "- None" when the slice is empty (avoids blank sections in the file).
fn markdown_list(items: &[String]) -> String {
    if items.is_empty() {
        return "- None".to_string();
    }

    // `iter().map(...).collect::<Vec<_>>().join(...)` is the idiomatic way
    // to transform a collection into a delimited string in Rust.
    items
        .iter()
        .map(|item| format!("- {item}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// =============================================================================
// Learning Notes
// =============================================================================
// • Matching on io::ErrorKind — lets you handle "file not found" differently
//   from "permission denied" without parsing error messages; always prefer
//   this over string matching.
// • `format!("{current}\n\n{entry}")` — Rust's format macro works like
//   Python's f-strings.  Variables inside `{}` are formatted in place.
// • `unwrap_or_else(|| ...)` — only evaluates the closure when the Option is
//   None; prefer this over `unwrap_or(expr)` when computing the fallback is
//   expensive (here it's just a clone, but the habit is good).
// • Building strings incrementally with `Vec<String>` then `join` is often
//   cleaner than repeated `push_str` calls on a single `String`.
