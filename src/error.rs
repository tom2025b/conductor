// =============================================================================
// error.rs — Shared error type for Conductor
//
// Rust requires every error to be a concrete type that implements the `Error`
// trait.  Instead of writing that boilerplate by hand, we use `thiserror`:
// the `#[derive(Error)]` macro generates the trait implementation from the
// `#[error("...")]` format string you put on each variant.
//
// Having one central error type (WorkflowError) means:
//  • Functions that can fail in multiple ways return Result<T, WorkflowError>.
//  • Callers can match on the exact variant to decide what to show the user.
//  • `#[source]` automatically wires up the "caused by" chain in error output.
// =============================================================================

use std::path::PathBuf;

// `thiserror::Error` is a derive macro — the compiler expands it into a full
// `impl std::error::Error for WorkflowError { ... }` block for us.
use thiserror::Error;

// Each variant represents a distinct failure mode.
// The `#[error("...")]` string is what gets shown when the error is printed.
#[derive(Debug, Error)]
pub enum WorkflowError {
    // Failed to open / read the YAML file from disk.
    // `{path}` and `{source}` are filled in from the struct fields.
    #[error("failed to read workflow file `{path}`")]
    ReadFile {
        path: PathBuf,
        // `#[source]` tells thiserror (and the standard Error trait) that
        // `source` is the underlying cause; it's surfaced by `.source()`.
        #[source]
        source: std::io::Error,
    },

    // File was read successfully but the YAML wasn't valid / didn't match our model.
    #[error("failed to parse workflow yaml `{path}`")]
    ParseYaml {
        path: PathBuf,
        #[source]
        source: serde_yaml::Error,
    },

    // The YAML was syntactically valid but failed semantic checks (e.g. a step
    // references an agent name that doesn't exist in the agents map).
    // The String contains all collected issues joined with newlines.
    #[error("workflow validation failed:\n{0}")]
    Validation(String),

    // A problem at runtime that isn't tied to a specific step —
    // e.g. "workflow contains no steps" or "exceeded max_steps".
    #[error("workflow execution failed: {0}")]
    Execution(String),

    // A specific step produced a non-zero exit code or the agent rejected it.
    #[error("step `{step_id}` failed: {message}")]
    StepFailed { step_id: String, message: String },
}

// =============================================================================
// Learning Notes
// =============================================================================
// • `thiserror` vs `anyhow`: use thiserror when you own a library error type
//   with distinct variants callers might want to match; use anyhow in
//   application/main code where you only need to propagate + display errors.
// • Named struct variants (ReadFile, ParseYaml) let you carry context like
//   the file path alongside the underlying IO error — much more debuggable
//   than a bare string.
// • `#[source]` chains errors so tools like `anyhow` can print "caused by" lines.
