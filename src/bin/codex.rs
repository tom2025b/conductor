// Codex companion binary.
//
// Intercepts `codex regen` (and `codex ';;regen'`) and runs Conductor's regen
// logic directly. All other commands are forwarded transparently to the real
// Codex CLI binary found in PATH.

use std::path::PathBuf;
use std::process::{Command, ExitCode};

use anyhow::{Result, bail};
use clap::Parser;
use conductor::regen::{RegenCommand, print_regen_report};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("regen") | Some(";;regen") => run_regen(&args[1..]),
        _ => forward_to_real_codex(&args),
    }
}

// --- regen ---

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
    let argv = std::iter::once("codex-regen").chain(extra_args.iter().map(String::as_str));

    // Use try_parse_from so --help and --version exit cleanly via e.exit()
    // rather than being swallowed by our error handler.
    let opts = match RegenArgs::try_parse_from(argv) {
        Ok(opts) => opts,
        Err(e) => e.exit(),
    };

    let report = RegenCommand {
        workflow_path: opts.workflow,
        project_dir: opts.project_dir,
        force: opts.force,
    }
    .run()?;

    print_regen_report(report);
    Ok(())
}

// --- forwarding ---

fn forward_to_real_codex(args: &[String]) -> Result<()> {
    let codex = find_real_codex()?;
    let status = Command::new(&codex)
        .args(args)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run `{}`: {e}", codex.display()))?;

    if !status.success() {
        bail!("codex exited with {status}");
    }
    Ok(())
}

// Walk PATH entries in order, returning the first `codex` executable that is
// not this binary itself. This avoids both infinite recursion and the need for
// any environment-variable configuration.
fn find_real_codex() -> Result<PathBuf> {
    let current = std::env::current_exe().ok();

    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join("codex");
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
