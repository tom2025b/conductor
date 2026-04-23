// =============================================================================
// lib.rs — Conductor library root
//
// Conductor is split into a library crate (`conductor`) and one or more binary
// crates (`src/main.rs`, `src/bin/codex.rs`).  This file is the root of the
// library — it declares which modules exist and makes them visible to callers.
//
// Splitting into lib + bin means:
//  • integration tests in `tests/` can import from `conductor::` directly.
//  • the `codex` companion binary can reuse the regen logic without copy-pasting.
//  • the library can be published to crates.io independently of the binaries.
// =============================================================================

// Each `pub mod` line tells Rust: "there is a subdirectory (or file) called
// <name> that is part of this crate, and external code is allowed to see it."
pub mod cli;       // CLI argument definitions (clap structs)
pub mod error;     // Shared error type (WorkflowError)
pub mod regen;     // Project analysis and workflow.yaml generation
pub mod runner;    // Workflow execution engine
pub mod workflow;  // YAML model, loader, and validator
