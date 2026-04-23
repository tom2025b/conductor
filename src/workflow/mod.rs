// =============================================================================
// workflow/mod.rs — Public interface for the workflow module
//
// The `workflow` directory contains three sub-modules:
//   model    — the Rust structs that mirror the YAML structure (data model)
//   loader   — reads a YAML file from disk and deserializes it into the model
//   validate — checks the loaded model for semantic correctness
//
// This mod.rs file does two things:
//   1. Declares the private sub-modules (`mod loader; mod model; mod validate;`)
//      so the Rust compiler knows to look for those files.
//   2. Re-exports the types that the rest of the crate (and tests) need via
//      `pub use`, so callers can write `conductor::workflow::Workflow` instead
//      of `conductor::workflow::model::Workflow`.
// =============================================================================

// `mod` without `pub` means "this sub-module exists but is private to this module".
// Other crates/modules that say `use conductor::workflow::loader` will get an error.
mod loader;
mod model;
mod validate;

// `pub use` re-exports: make the loader function and all model types accessible
// from `conductor::workflow::*` without exposing the internal file layout.
pub use loader::load_workflow;
pub use model::{
    AgentConfig, ConductorAgent, HandoffConfig, HandoffMode, ReviewGate, ReviewRequirement,
    Snippet, Step, StepRoute, StepTransition, Workflow, WorkflowDefaults,
};
