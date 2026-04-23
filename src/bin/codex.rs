// =============================================================================
// bin/codex.rs — Codex companion binary
//
// This is a thin wrapper that sits *in front of* the real `codex` CLI binary.
// When it is installed as `codex` earlier in PATH it intercepts two subcommands:
//
//   codex regen [opts]    — runs Conductor's workflow.yaml generator
//   codex ;;regen [opts]  — same (alias used inside Claude Code sessions)
//
// Every other subcommand is forwarded transparently to the real `codex` binary
// found later in PATH — so this shim is completely invisible for normal Codex use.
//
// Why this exists:
//   OpenAI Codex doesn't natively support `regen`.  By providing this shim,
//   a `codex regen` call from inside any terminal just works, regardless of
//   whether the user is thinking about Conductor or not.
// =============================================================================

use std::path::PathBuf;
use std::process::{Command, ExitCode}; // ExitCode is the idiomatic return type for main()

use anyhow::{Result, bail};
use clap::Parser;
use conductor::regen::{RegenCommand, print_regen_report};

fn main() -> ExitCode {
    // Wrap the logic in a separate `run()` that returns a Result, then
    // handle the error in main() by printing and returning FAILURE.
    // This avoids a nested `match` or `unwrap()` in main.
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    // `skip(1)` drops argv[0] (the binary name itself), giving us just the args.
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Route based on the first argument.
    match args.first().map(String::as_str) {
        // Intercept `regen` and `;;regen` and run our logic.
        Some("regen") | Some(";;regen") => run_regen(&args[1..]),
        // Everything else (exec, login, etc.) gets forwarded to the real binary.
        _ => forward_to_real_codex(&args),
    }
}

// ─── Regen sub-command ────────────────────────────────────────────────────────

/// Argument struct for `codex regen` — parsed by clap from the remaining argv.
#[derive(Debug, Parser)]
#[command(
    name = "codex regen",
    about = "Generate a workflow.yaml for the current project"
)]
struct RegenArgs {
    /// Project directory to analyze. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    project_dir: Option<PathBuf>,

    /// Path to write the generated workflow file.
    #[arg(long, default_value = "workflow.yaml")]
    workflow: PathBuf,

    /// Overwrite an existing workflow file without prompting.
    #[arg(long)]
    force: bool,
}

fn run_regen(extra_args: &[String]) -> Result<()> {
    // Prepend a fake argv[0] so clap sees a well-formed command line.
    // `std::iter::once` creates an iterator over a single value; `chain` appends
    // the real args, giving us `["codex-regen", ...extra_args...]`.
    let argv = std::iter::once("codex-regen").chain(extra_args.iter().map(String::as_str));

    // `try_parse_from` is used instead of `parse_from` so that --help and
    // --version print and exit via `e.exit()` rather than returning an Err
    // that our outer error handler would swallow with a generic message.
    let opts = match RegenArgs::try_parse_from(argv) {
        Ok(opts) => opts,
        Err(e) => e.exit(), // clap calls process::exit() internally here
    };

    // Delegate to the shared regen logic in the library crate.
    let report = RegenCommand {
        workflow_path: opts.workflow,
        project_dir:   opts.project_dir,
        force:         opts.force,
    }
    .run()?;

    print_regen_report(report);
    Ok(())
}

// ─── Forwarding ───────────────────────────────────────────────────────────────

/// Forward all non-regen arguments to the real `codex` binary.
fn forward_to_real_codex(args: &[String]) -> Result<()> {
    let codex = find_real_codex()?;

    // `.status()` (not `.output()`) streams the child process's I/O to our own
    // stdin/stdout/stderr — the user sees it exactly as they would if they ran
    // `codex` directly.
    let status = Command::new(&codex)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run `{}`: {e}", codex.display()))?;

    if !status.success() {
        bail!("codex exited with {status}");
    }
    Ok(())
}

/// Walk PATH entries in order and return the first `codex` executable that is
/// NOT this binary itself.
///
/// Checking `current_exe()` prevents infinite recursion when this shim is
/// installed as `codex` — we skip ourselves and keep walking to the real binary.
fn find_real_codex() -> Result<PathBuf> {
    // `current_exe()` gives us our own path (may fail on some platforms, hence `ok()`).
    let current = std::env::current_exe().ok();

    if let Ok(path_var) = std::env::var("PATH") {
        // `split_paths` handles platform-specific path separators (`:` on Unix, `;` on Windows).
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("codex");
            // `is_file()` returns false for directories and non-existent paths.
            if candidate.is_file() && current.as_deref() != Some(candidate.as_path()) {
                return Ok(candidate);
            }
        }
    }

    bail!(
        "no `codex` binary found in PATH\n\
         Install the Codex CLI (https://github.com/openai/codex) to forward non-regen commands."
    )
}

// =============================================================================
// Learning Notes
// =============================================================================
// • `ExitCode` return type — the idiomatic way to exit with a specific code
//   from main() in Rust 1.61+.  Avoids `std::process::exit()` which bypasses
//   destructors (Drop impls) for any live values.
// • `std::iter::once(x).chain(iter)` — prepend a single item to an iterator
//   without allocating a Vec; common when you need to inject argv[0] for clap.
// • `Command::status()` vs `Command::output()` — status() inherits the parent's
//   I/O (user sees the child's terminal output directly); output() captures it.
//   Use status() when forwarding, output() when you need to process the text.
// • `current_exe()` — resolves the path of the running binary; used here to
//   detect and skip ourselves when searching PATH.
