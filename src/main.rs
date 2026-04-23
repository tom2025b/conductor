// =============================================================================
// main.rs — Conductor entry point
//
// This file is the first thing Rust runs when you type `conductor`.
// It parses the command-line arguments, calls the right sub-system, and
// prints a human-readable summary of the result.
//
// Three sub-commands are supported:
//   validate  — parse + validate a workflow YAML, no execution
//   run       — execute the workflow step by step using real or dry-run agents
//   regen     — inspect the current project and generate a workflow.yaml
// =============================================================================

// `anyhow::Result` is a convenient alias for `Result<T, anyhow::Error>`.
// `anyhow` auto-wraps any error type so you can use `?` without matching
// every specific error variant — great for main() and CLI code.
use anyhow::Result;

// `clap::Parser` provides the `parse()` method that turns argv into a struct.
use clap::Parser;

// Bring in our own types from the library portion of this crate (`conductor::...`).
use conductor::cli::{Cli, ConductorCommand};
use conductor::regen::{RegenCommand, print_regen_report};
use conductor::runner::{RunStatus, Runner, RunnerOptions};
use conductor::workflow::load_workflow;

// tracing_subscriber wires up structured logging (the `info!`, `debug!` macros)
// to something that actually writes to stderr.
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

fn main() -> Result<()> {
    // `Cli::parse()` reads `std::env::args()` and populates our Cli struct.
    // If --help or --version is requested, clap prints and exits automatically.
    let cli = Cli::parse();

    // Set up log filtering based on the -v / -vv flags the user passed.
    init_tracing(cli.verbose);

    // `unwrap_or(Default)` means "if no subcommand was given, run Validate".
    // This lets users type just `conductor` to quickly check their YAML.
    match cli.command.unwrap_or(ConductorCommand::Validate) {
        ConductorCommand::Validate => {
            // Load and validate the YAML; bail on error via `?`.
            let workflow = load_workflow(&cli.workflow)?;
            print_validation_summary(&workflow, cli.summary);
        }

        ConductorCommand::Run { live, max_steps } => {
            let workflow = load_workflow(&cli.workflow)?;

            // `dry_run: !live` flips the sense: --live means "really do it".
            // Without --live, the runner simulates every step (safe by default).
            let runner = Runner::new(
                workflow,
                &cli.workflow,
                RunnerOptions {
                    dry_run: !live,
                    max_steps,
                },
            );

            // `.run()` returns an `ExecutionReport` describing every step taken.
            let report = runner.run()?;
            print_execution_report(&report);
        }

        ConductorCommand::Regen { project_dir, force } => {
            // Package the CLI options into a `RegenCommand` value object
            // and hand off all the logic to the regen module.
            let command = RegenCommand {
                workflow_path: cli.workflow,
                project_dir,
                force,
            };
            let report = command.run()?;
            print_regen_report(report);
        }
    }

    Ok(())
}

// Configure the tracing subscriber's log level filter.
// Priority: the RUST_LOG env-var (if set) overrides the -v flags.
fn init_tracing(verbosity: u8) {
    // Map 0/1/2+ flags to a level name string that EnvFilter understands.
    let fallback = match verbosity {
        0 => "warn",  // default: only warnings and errors
        1 => "info",  // -v: add info messages
        _ => "debug", // -vv or more: full debug output
    };

    // `try_from_default_env` reads RUST_LOG; falls back to our computed level.
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(fallback));

    // `registry()` + `with()` chain lets you layer multiple subscribers.
    // `fmt::layer()` writes formatted lines to stderr.
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(env_filter)
        .init();
}

fn print_validation_summary(workflow: &conductor::workflow::Workflow, summary: bool) {
    // Basic one-liner that tells the user their YAML is structurally sound.
    println!(
        "validated workflow `{}` with {} step(s), {} agent(s), {} review gate(s)",
        workflow.name,
        workflow.steps.len(),
        workflow.agents.len(),
        workflow.review_gates.len()
    );

    // If --summary was passed, dump the handoff path and step→agent mapping.
    if summary {
        println!("handoff file: {}", workflow.handoff.path);
        for step in &workflow.steps {
            println!("step `{}` -> agent `{}`", step.id, step.agent);
        }
    }
}

fn print_execution_report(report: &conductor::runner::ExecutionReport) {
    println!(
        "executed workflow `{}` in {} mode",
        report.workflow_name,
        if report.dry_run { "dry-run" } else { "live" }
    );

    // Print each completed step with its agent and whether it was reviewed.
    for step in &report.completed_steps {
        println!(
            "step `{}` via `{}` => {:?}{}",
            step.step_id,
            step.agent,
            step.status,
            if step.reviewed { " (reviewed)" } else { "" }
        );
    }

    // The final workflow status: Completed (last step returned), Halted (route: halt),
    // or Failed (a step or review gate rejected the run).
    match &report.final_status {
        RunStatus::Completed => println!("workflow status: completed"),
        RunStatus::Halted => println!("workflow status: halted"),
        RunStatus::Failed(message) => println!("workflow status: failed: {message}"),
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
// • `anyhow::Result` / `?` — zero-boilerplate error propagation; converts any
//   error into a boxed trait object automatically.
// • `clap::Parser` derive macro — generates argument parsing from field types
//   and doc-comments at compile time; no runtime parsing code needed.
// • `tracing_subscriber::EnvFilter` — lets users override log verbosity with
//   RUST_LOG=conductor=debug without recompiling the binary.
// • Matching on an `Option<Enum>` with `unwrap_or` gives a clean default
//   sub-command without a nested if/else chain.
