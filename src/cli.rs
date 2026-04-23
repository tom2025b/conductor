// =============================================================================
// cli.rs — Command-line interface definitions
//
// This module defines what the user can type on the command line.
// We use the `clap` crate with its "derive" feature: you annotate a struct
// with `#[derive(Parser)]` and clap generates all the parsing logic for you.
//
// Two structs live here:
//   Cli              — top-level flags shared by every sub-command
//   ConductorCommand — the sub-commands (validate / run / regen)
// =============================================================================

use std::path::PathBuf; // `PathBuf` is the owned, heap-allocated path type (like String vs &str).

// `ArgAction::Count` lets a flag be repeated (-v -v or -vv) and counts them.
use clap::{ArgAction, Parser, Subcommand};

// `#[derive(Parser)]` on a struct tells clap to build argument parsing from
// the struct's fields and their doc-comments (`///`).
// `#[command(...)]` attributes configure the help text and binary name.
#[derive(Debug, Clone, Parser)]
#[command(
    name = "conductor",
    author,    // reads `authors` from Cargo.toml
    version,   // reads `version` from Cargo.toml
    about = "Coordinate multi-agent workflows across Claude Code and OpenAI Codex",
    long_about = None
)]
pub struct Cli {
    // `#[command(subcommand)]` tells clap that one field holds the sub-command enum.
    // `Option<>` makes it optional; we default to Validate in main.rs.
    #[command(subcommand)]
    pub command: Option<ConductorCommand>,

    /// Path to the workflow definition file.
    // `short` generates -w, `long` generates --workflow.
    // `env` means the value can also come from the CONDUCTOR_WORKFLOW env var.
    // `default_value` is used when neither flag nor env-var is provided.
    #[arg(
        short,
        long,
        value_name = "FILE",
        env = "CONDUCTOR_WORKFLOW",
        default_value = "workflow.yaml"
    )]
    pub workflow: PathBuf,

    /// Emit workflow details after a successful validation.
    #[arg(long)]
    pub summary: bool,

    /// Increase log verbosity (`-v`, `-vv`).
    // `ArgAction::Count` turns -v into 1, -vv into 2, etc.
    // The field type `u8` holds the count.
    #[arg(short, long, action = ArgAction::Count)]
    pub verbose: u8,
}

// `#[derive(Subcommand)]` on an enum means each variant becomes a sub-command.
// The variant name (lowercased) is the sub-command name on the CLI.
#[derive(Debug, Clone, Subcommand)]
pub enum ConductorCommand {
    /// Validate the workflow definition and print an optional summary.
    Validate,

    /// Execute the workflow step-by-step with the configured agents.
    Run {
        /// Execute real agent calls instead of the default dry-run simulation.
        // Without `--live`, the runner only simulates steps — safe for testing.
        #[arg(long)]
        live: bool,

        /// Limit the number of executed steps to prevent runaway loops.
        // `default_value_t` sets the default to the Rust value 32.
        #[arg(long, default_value_t = 32)]
        max_steps: usize,
    },

    /// Analyze the current project and generate a workflow.yaml.
    // `alias` lets users type `codex ;;regen` as an alternative name.
    #[command(alias = ";;regen")]
    Regen {
        /// Project directory to analyze. Defaults to the current directory.
        #[arg(long, value_name = "DIR")]
        project_dir: Option<PathBuf>,

        /// Overwrite the target workflow file without prompting.
        #[arg(long)]
        force: bool,
    },
}

// =============================================================================
// Learning Notes
// =============================================================================
// • clap's derive API: annotate a struct with `#[derive(Parser)]` and clap
//   reads field names, doc-comments, and `#[arg]` attributes at compile time
//   to produce a fully-featured arg parser — no hand-written logic needed.
// • `PathBuf` vs `&Path`: PathBuf owns its memory (like String owns chars),
//   while `&Path` is a borrowed slice (like &str). Use PathBuf when you need
//   to store or return a path; use &Path when you just need to read one.
// • `Option<T>` for CLI fields: using Option lets the argument be absent,
//   with None indicating "user didn't supply it" — different from a default value.
