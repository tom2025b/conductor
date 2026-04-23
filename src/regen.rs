// =============================================================================
// regen.rs — Project analysis and workflow.yaml generation
//
// `conductor regen` (or `codex regen`) inspects the current project directory
// and generates a tailored workflow.yaml without requiring you to write it by hand.
//
// The pipeline is:
//   1. Detect the project type (Rust CLI, Rust library, Node, Python, generic)
//      by looking for well-known manifest files (Cargo.toml, package.json, etc.)
//   2. Ask the user if they want to inject custom preferences into the YAML.
//   3. Render the YAML template, filling in project-specific values.
//   4. Ask the user before overwriting an existing workflow file (unless --force).
//   5. Write the file to disk and report what happened.
//
// Key design decision: the rendered YAML is built by a format!() string template
// rather than a serialization round-trip through the model structs.  This
// preserves YAML comments and gives us full control over whitespace / order.
// =============================================================================

use std::fs;
use std::io::{self, Write}; // `io::Write` is needed for flush() before reading stdin
use std::path::{Path, PathBuf};

// `anyhow` crate: `bail!` is a macro that returns Err(anyhow::Error) with a message.
// `Context` adds `.with_context(|| "...")` to Result for better error messages.
use anyhow::{Context, Result, bail};

// ─── Public types ─────────────────────────────────────────────────────────────

/// All the inputs for a single regen invocation.
#[derive(Debug, Clone)]
pub struct RegenCommand {
    /// Where to write the generated workflow file.
    pub workflow_path: PathBuf,

    /// Project directory to inspect. `None` → use the current directory.
    pub project_dir: Option<PathBuf>,

    /// Skip the "file already exists, overwrite?" prompt.
    pub force: bool,
}

/// Summary of what regen produced — printed by the caller.
#[derive(Debug, Clone)]
pub struct RegenReport {
    pub workflow_path: PathBuf,
    pub project_root: PathBuf,
    pub project_kind: &'static str, // `&'static str` = string literal stored in binary, no allocation
    pub notes: Vec<String>,
}

// ─── Internal types ───────────────────────────────────────────────────────────

/// All the facts we discovered about the project, ready to be rendered into YAML.
#[derive(Debug, Clone)]
struct ProjectAnalysis {
    root: PathBuf,
    name: String,
    kind: ProjectKind,
    notes: Vec<String>,          // human-readable observations (shown before writing)
    validation_commands: Vec<String>, // shell commands to embed in the `run:` block
}

/// Supported project type taxonomy.
/// `Copy` lets us pass this by value (no `&`/`clone()` needed) since it fits in a byte.
#[derive(Debug, Clone, Copy)]
enum ProjectKind {
    RustCli,
    RustLibrary,
    NodeApp,
    PythonApp,
    Generic,
}

impl ProjectKind {
    /// Convert to the string used in the generated YAML and in reports.
    fn as_str(self) -> &'static str {
        match self {
            Self::RustCli     => "rust-cli",
            Self::RustLibrary => "rust-library",
            Self::NodeApp     => "node-app",
            Self::PythonApp   => "python-app",
            Self::Generic     => "generic",
        }
    }
}

// ─── RegenCommand implementation ─────────────────────────────────────────────

impl RegenCommand {
    /// Run the full regen pipeline and return a summary report.
    pub fn run(self) -> Result<RegenReport> {
        // Convert relative paths to absolute so everything is deterministic.
        let workflow_path = absolutize(&self.workflow_path)?;
        let project_root = match self.project_dir {
            Some(path) => absolutize(path)?,
            // `current_dir()` returns the process's working directory.
            None => std::env::current_dir()?,
        };

        // Step 1: inspect the project directory.
        let analysis = analyze_project(&project_root)?;

        // Step 2: show what we found and where the file will be written.
        print_analysis(&analysis, &workflow_path);

        // Step 3: optionally collect custom preferences to embed in the YAML.
        let custom_preferences = prompt_for_preferences()?;

        // Step 4: render the YAML template string.
        let yaml = render_workflow_yaml(&analysis, custom_preferences.as_deref());

        // Step 5: confirm before overwriting an existing file (unless --force).
        if workflow_path.exists() && !self.force {
            let overwrite = prompt_yes_no(
                &format!(
                    "workflow file `{}` already exists. Overwrite it?",
                    workflow_path.display()
                ),
                false, // default answer is "no" — safe default
            )?;
            if !overwrite {
                bail!("aborted without writing workflow file");
            }
        }

        // Step 6: ensure the parent directory exists, then write the file.
        if let Some(parent) = workflow_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create workflow directory `{}`", parent.display())
            })?;
        }

        fs::write(&workflow_path, yaml).with_context(|| {
            format!(
                "failed to write workflow file `{}`",
                workflow_path.display()
            )
        })?;

        Ok(RegenReport {
            workflow_path,
            project_root: analysis.root,
            project_kind: analysis.kind.as_str(),
            notes: analysis.notes,
        })
    }
}

/// Print the regen report to stdout.  Called by main.rs and the codex binary.
pub fn print_regen_report(report: RegenReport) {
    println!(
        "generated workflow `{}` for detected project type `{}`",
        report.workflow_path.display(),
        report.project_kind
    );
    println!("project root: {}", report.project_root.display());
    for note in report.notes {
        println!("note: {note}");
    }
}

// ─── Project analysis ─────────────────────────────────────────────────────────

/// Walk the project root looking for manifest files and return a ProjectAnalysis.
fn analyze_project(project_root: &Path) -> Result<ProjectAnalysis> {
    if !project_root.is_dir() {
        bail!(
            "project directory `{}` does not exist",
            project_root.display()
        );
    }

    // These are the candidate manifest files we look for.
    let cargo_toml   = project_root.join("Cargo.toml");
    let package_json = project_root.join("package.json");
    let pyproject    = project_root.join("pyproject.toml");
    let src_main_rs  = project_root.join("src/main.rs");
    let src_lib_rs   = project_root.join("src/lib.rs");

    let mut notes = Vec::new();

    // Determine the project kind and its canonical name.
    // We check manifests in order of priority: Rust > Node > Python > generic.
    let (kind, name) = if cargo_toml.exists() {
        // Read Cargo.toml to extract the `name` field from `[package]`.
        let manifest = fs::read_to_string(&cargo_toml)
            .with_context(|| format!("failed to read `{}`", cargo_toml.display()))?;

        // Fall back to the directory name if we can't find `name = "..."` in the manifest.
        let package_name = parse_package_name(&manifest).unwrap_or_else(|| {
            project_root
                .file_name()
                .and_then(|value| value.to_str()) // OsStr → &str (None if not valid UTF-8)
                .unwrap_or("project")
                .to_string()
        });

        // Presence of src/main.rs distinguishes a binary crate from a library.
        if src_main_rs.exists() {
            notes.push("detected Rust crate with binary entrypoint".to_string());
            (ProjectKind::RustCli, package_name)
        } else if src_lib_rs.exists() {
            notes.push("detected Rust crate with library entrypoint".to_string());
            (ProjectKind::RustLibrary, package_name)
        } else {
            notes.push("detected Cargo manifest without standard src entrypoint".to_string());
            (ProjectKind::RustLibrary, package_name)
        }
    } else if package_json.exists() {
        notes.push("detected Node project from package.json".to_string());
        (
            ProjectKind::NodeApp,
            project_root
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("project")
                .to_string(),
        )
    } else if pyproject.exists() {
        notes.push("detected Python project from pyproject.toml".to_string());
        (
            ProjectKind::PythonApp,
            project_root
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("project")
                .to_string(),
        )
    } else {
        notes.push("no dominant project manifest found; generating a generic workflow".to_string());
        (
            ProjectKind::Generic,
            project_root
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("project")
                .to_string(),
        )
    };

    // A .git directory means git is available — useful context for agents.
    if project_root.join(".git").exists() {
        notes.push("git repository detected".to_string());
    }

    // Detect the appropriate CI/validation commands for this project type.
    let validation_commands = detect_validation_commands(project_root, kind)?;

    Ok(ProjectAnalysis {
        root: project_root.to_path_buf(),
        name,
        kind,
        notes,
        validation_commands,
    })
}

// ─── YAML template renderer ───────────────────────────────────────────────────

/// Render the workflow.yaml content as a string.
///
/// We use a `format!()` template rather than serializing through the model
/// structs because:
///   • We can preserve block scalar style (`|`) for multi-line strings.
///   • We can emit comments (serde_yaml strips them).
///   • The output order is stable and predictable.
fn render_workflow_yaml(analysis: &ProjectAnalysis, custom_preferences: Option<&str>) -> String {
    // Pick a description appropriate to the project type.
    let description = match analysis.kind {
        ProjectKind::RustCli     => "Coordinate implementation, review, and validation for a Rust CLI project.",
        ProjectKind::RustLibrary => "Coordinate implementation, review, and validation for a Rust library project.",
        ProjectKind::NodeApp     => "Coordinate implementation, review, and validation for a Node application.",
        ProjectKind::PythonApp   => "Coordinate implementation, review, and validation for a Python project.",
        ProjectKind::Generic     => "Coordinate implementation, review, and validation for a software project.",
    };

    // Build the `run:` block (list of shell commands) for the implement step.
    let build_commands = build_commands_for_analysis(analysis);

    // Decide whether we need to inject a custom_preferences snippet.
    let has_custom_preferences = custom_preferences
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);

    // Render the custom_preferences snippet block (or empty string if none).
    let preferences_block = custom_preferences
        .filter(|value| !value.trim().is_empty())
        .map(render_preferences_block)
        .unwrap_or_default();

    // Render the `snippets:` lists for each step.
    // The plan step gets `project_brief`; review steps get `review_brief`.
    let plan_snippets     = render_step_snippets(&["project_brief"],  has_custom_preferences);
    let review_snippets   = render_step_snippets(&["review_brief"],   has_custom_preferences);
    let implement_snippets = render_step_snippets(&[],                has_custom_preferences);

    // `r#"..."#` is a raw string literal — backslashes inside are literal.
    // This avoids having to escape every `"` in the YAML template.
    format!(
        r#"version: "1"
name: "{name}-generated"
description: "{description}"

defaults:
  auto_handoff: true
  review_required: true
  working_directory: "."

handoff:
  path: "handover.md"
  mode: "append"
  required_sections:
    - "Current State"
    - "Open Questions"
    - "Next Actions"

snippets:
  project_brief:
    trigger: ";;project"
    description: "Project-specific delivery brief generated by Conductor regen."
    content: |
      You are working inside `{name}`.
      Project type: {kind}.
      Keep changes cohesive, preserve handoff quality, and leave the next agent with a clear state summary.
{preferences_block}  review_brief:
    trigger: ";;review"
    description: "Standard review request."
    content: |
      Review this change for correctness, regression risk, workflow integrity, and missing validation.

agents:
  claude:
    provider: "claude_code"
    model: "sonnet"
    profile: "implementer"
    workspace: "."
  codex:
    provider: "open_ai_codex"
    model: "gpt-5.4"
    profile: "reviewer"
    workspace: "."

steps:
  - id: "plan"
    title: "Analyze the task and prepare the execution plan"
    agent: "claude"
    prompt: |
      Inspect the repository, identify the files likely to change, and propose a clean implementation plan.
      Use ;;project.
{plan_snippets}    review:
      required: false
    on_success:
      handoff_to: "codex"
      next_step: "review_plan"
      route: "continue"

  - id: "review_plan"
    title: "Review the plan before implementation"
    agent: "codex"
    prompt: |
      Review the proposed plan, call out risks, and either approve it or force a retry.
      Use ;;review.
{review_snippets}    review:
      gate: "quality_gate"
      required: true
    on_success:
      handoff_to: "claude"
      next_step: "implement"
      route: "continue"
    on_failure:
      handoff_to: "claude"
      next_step: "plan"
      route: "retry"

  - id: "implement"
    title: "Implement the approved changes"
    agent: "claude"
    prompt: |
      Implement the approved plan in the repository. Keep the handoff concise and specific.
{implement_snippets}    run:
{build_commands}    review:
      gate: "quality_gate"
      required: true
    on_success:
      handoff_to: "codex"
      next_step: "final_review"
      route: "continue"

  - id: "final_review"
    title: "Approve the final result"
    agent: "codex"
    prompt: |
      Perform final review, verify the change is shippable, and confirm the handoff is complete.
{review_snippets}    review:
      gate: "release_gate"
      required: true
    on_success:
      route: "halt"

review_gates:
  - id: "quality_gate"
    name: "Quality Gate"
    required_approvers: 1
    instructions: "Validate design coherence, correctness, and project-specific checks."
  - id: "release_gate"
    name: "Release Gate"
    required_approvers: 1
    instructions: "Confirm the result is ready to merge or release."
"#,
        name             = sanitize_yaml_scalar(&analysis.name),
        description      = sanitize_yaml_scalar(description),
        kind             = analysis.kind.as_str(),
        build_commands   = build_commands,
        plan_snippets    = plan_snippets,
        review_snippets  = review_snippets,
        implement_snippets = implement_snippets,
        preferences_block  = preferences_block,
    )
}

/// Build the YAML `run:` block from the detected validation commands.
fn build_commands_for_analysis(analysis: &ProjectAnalysis) -> String {
    analysis
        .validation_commands
        .iter()
        // Each command becomes one YAML list item indented 6 spaces to match the template.
        .map(|command| format!("      - \"{}\"\n", sanitize_yaml_scalar(command)))
        .collect()
}

/// Render a `custom_preferences` snippet block with the user's text.
fn render_preferences_block(preferences: &str) -> String {
    let mut block = String::from("  custom_preferences:\n");
    block.push_str("    trigger: \";;prefs\"\n");
    block.push_str("    description: \"User-provided generation preferences.\"\n");
    block.push_str("    content: |\n");
    // Each line of the preferences text needs 6 spaces of indentation for
    // the YAML block scalar (`|`) to work correctly.
    for line in preferences.lines() {
        block.push_str("      ");
        block.push_str(line);
        block.push('\n');
    }
    block
}

/// Render a `snippets:` sub-block for a step.
/// Optionally appends `custom_preferences` to the list when the user provided them.
fn render_step_snippets(base_snippets: &[&str], include_custom_preferences: bool) -> String {
    // Collect into a Vec so we can conditionally push an extra entry.
    let mut snippets: Vec<&str> = base_snippets.to_vec();
    if include_custom_preferences {
        snippets.push("custom_preferences");
    }

    // An empty snippet list → return nothing (don't add a `snippets:` key at all).
    if snippets.is_empty() {
        return String::new();
    }

    let mut block = String::from("    snippets:\n");
    for snippet in snippets {
        block.push_str(&format!("      - \"{snippet}\"\n"));
    }
    block
}

// ─── Interactive prompts ──────────────────────────────────────────────────────

fn print_analysis(analysis: &ProjectAnalysis, workflow_path: &Path) {
    println!("analyzing project: {}", analysis.root.display());
    println!("detected project type: {}", analysis.kind.as_str());
    println!("workflow target: {}", workflow_path.display());
    for note in &analysis.notes {
        println!("signal: {note}");
    }
}

/// Ask the user if they want to add custom preferences, then collect them.
fn prompt_for_preferences() -> Result<Option<String>> {
    let wants_preferences = prompt_yes_no(
        "Do you want to add custom preferences before writing workflow.yaml?",
        false,
    )?;

    if !wants_preferences {
        return Ok(None);
    }

    println!("Enter custom preferences. Submit an empty line to finish.");
    let mut collected = Vec::new();
    loop {
        let line = prompt_line("> ")?;
        if line.trim().is_empty() {
            break; // empty line signals end of input
        }
        collected.push(line);
    }

    if collected.is_empty() {
        Ok(None)
    } else {
        // Join the lines back together with newlines for the YAML block scalar.
        Ok(Some(collected.join("\n")))
    }
}

/// Prompt for a yes/no answer, with a configurable default.
///
/// `default` is returned when the user presses Enter without typing anything.
fn prompt_yes_no(message: &str, default: bool) -> Result<bool> {
    // Show "[Y/n]" when default is true (uppercase = default option).
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    let input = prompt_line(&format!("{message} {suffix} "))?;
    let trimmed = input.trim().to_ascii_lowercase();

    if trimmed.is_empty() {
        return Ok(default);
    }

    match trimmed.as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no"  => Ok(false),
        _ => bail!("expected yes or no"),
    }
}

/// Print a prompt and read one line of input from stdin.
///
/// `io::stdout().flush()` ensures the prompt text is visible before we block
/// waiting for input.  Without flush(), buffered I/O might delay the prompt
/// until after the user has already typed their answer.
fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().context("failed to flush stdout")?;

    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("failed to read user input")?;

    // Strip the trailing newline (and \r on Windows) before returning.
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

// ─── Path helpers ─────────────────────────────────────────────────────────────

/// Convert a possibly-relative path to an absolute path.
///
/// Unlike `std::fs::canonicalize`, this does NOT require the path to exist —
/// we may be building the path to a file we're about to create.
fn absolutize(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        // Join to the current directory to make it absolute.
        Ok(std::env::current_dir()?.join(path))
    }
}

// ─── Manifest parsers ─────────────────────────────────────────────────────────

/// Extract `name = "..."` from a Cargo.toml file content.
///
/// This is a hand-written line scanner rather than a full TOML parser to keep
/// the dependency surface small.  It looks for `name = "..."` inside `[package]`.
fn parse_package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with('[') {
            // Track which section we're in.  `[workspace]` or `[dependencies]` etc.
            // resets `in_package` to false.
            in_package = trimmed == "[package]";
            continue;
        }

        if in_package && trimmed.starts_with("name") {
            // Split on `=` and clean up quotes from the value side.
            let (_, value) = trimmed.split_once('=')?;
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// Escape characters that are special inside a YAML double-quoted scalar.
///
/// This prevents user-controlled strings (like a project name with quotes or
/// backslashes) from breaking the generated YAML structure.
fn sanitize_yaml_scalar(value: &str) -> String {
    value
        .replace('\\', "\\\\")  // backslash must be doubled
        .replace('"', "\\\"")   // quotes must be escaped
        .replace('\n', "\\n")   // literal newlines would break the scalar
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

// ─── Validation command detection ─────────────────────────────────────────────

fn detect_validation_commands(project_root: &Path, kind: ProjectKind) -> Result<Vec<String>> {
    let commands = match kind {
        ProjectKind::RustCli | ProjectKind::RustLibrary => detect_rust_commands(project_root)?,
        ProjectKind::NodeApp   => detect_node_commands(project_root)?,
        ProjectKind::PythonApp => detect_python_commands(project_root),
        ProjectKind::Generic   => vec!["echo 'add project-specific validation commands'".to_string()],
    };
    Ok(commands)
}

/// Detect the appropriate Rust CI commands by inspecting Cargo.toml.
fn detect_rust_commands(project_root: &Path) -> Result<Vec<String>> {
    let manifest = fs::read_to_string(project_root.join("Cargo.toml")).with_context(|| {
        format!(
            "failed to read `{}`",
            project_root.join("Cargo.toml").display()
        )
    })?;

    // `cargo fmt` is always useful to normalize style before review.
    let mut commands = vec!["cargo fmt".to_string()];

    // Use `--workspace` when the manifest defines a Cargo workspace.
    if manifest.contains("[workspace]") {
        commands.push("cargo test --workspace".to_string());
    } else {
        commands.push("cargo test".to_string());
    }

    // Add clippy if the project has configured it.
    if manifest.contains("clippy") || project_root.join("clippy.toml").exists() {
        // Insert clippy *before* tests so lint failures are caught early.
        commands.insert(
            1,
            "cargo clippy --all-targets --all-features -- -D warnings".to_string(),
        );
    }

    Ok(commands)
}

/// Detect Node.js CI commands by inspecting package.json for script names.
fn detect_node_commands(project_root: &Path) -> Result<Vec<String>> {
    let package_json =
        fs::read_to_string(project_root.join("package.json")).with_context(|| {
            format!(
                "failed to read `{}`",
                project_root.join("package.json").display()
            )
        })?;

    let mut commands = Vec::new();

    // Check for well-known script names (naive string search is fine here).
    if package_json.contains("\"lint\"")  { commands.push("npm run lint".to_string()); }
    if package_json.contains("\"test\"")  { commands.push("npm test".to_string()); }
    if package_json.contains("\"build\"") { commands.push("npm run build".to_string()); }

    // Fallback to `npm test` if no scripts were detected.
    if commands.is_empty() {
        commands.push("npm test".to_string());
    }

    Ok(commands)
}

/// Detect Python CI commands based on tool config files.
fn detect_python_commands(project_root: &Path) -> Vec<String> {
    let mut commands = Vec::new();

    // Ruff is a fast Python linter; detect it from its config file.
    if project_root.join("ruff.toml").exists() || project_root.join(".ruff.toml").exists() {
        commands.push("ruff check .".to_string());
    }

    // pytest is the de-facto standard test runner.
    commands.push("pytest".to_string());
    commands
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rust_cli_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src")).expect("src");
        fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\n",
        )
        .expect("cargo");
        fs::write(dir.path().join("src/main.rs"), "fn main() {}\n").expect("main");

        let analysis = analyze_project(dir.path()).expect("analysis");
        assert_eq!(analysis.kind.as_str(), "rust-cli");
        assert_eq!(analysis.name, "demo");
    }

    #[test]
    fn renders_preferences_snippet_when_present() {
        let analysis = ProjectAnalysis {
            root: PathBuf::from("/tmp/demo"),
            name: "demo".to_string(),
            kind: ProjectKind::RustCli,
            notes: Vec::new(),
            validation_commands: vec!["cargo fmt".to_string(), "cargo test".to_string()],
        };

        let yaml = render_workflow_yaml(&analysis, Some("Prefer small commits"));
        assert!(yaml.contains("custom_preferences"));
        assert!(yaml.contains("Prefer small commits"));
        assert!(yaml.contains("\"cargo test\""));
        assert!(
            yaml.contains("snippets:\n      - \"project_brief\"\n      - \"custom_preferences\"")
        );
        assert!(
            yaml.contains("snippets:\n      - \"review_brief\"\n      - \"custom_preferences\"")
        );
        assert!(yaml.contains(
            "prompt: |\n      Implement the approved plan in the repository. Keep the handoff concise and specific.\n    snippets:\n      - \"custom_preferences\"\n    run:"
        ));
    }

    #[test]
    fn omits_preferences_snippet_when_not_present() {
        let analysis = ProjectAnalysis {
            root: PathBuf::from("/tmp/demo"),
            name: "demo".to_string(),
            kind: ProjectKind::RustCli,
            notes: Vec::new(),
            validation_commands: vec!["cargo fmt".to_string(), "cargo test".to_string()],
        };

        let yaml = render_workflow_yaml(&analysis, None);
        assert!(!yaml.contains("custom_preferences"));
        assert!(yaml.contains("snippets:\n      - \"project_brief\""));
        assert!(yaml.contains("snippets:\n      - \"review_brief\""));
        assert!(!yaml.contains(
            "prompt: |\n      Implement the approved plan in the repository. Keep the handoff concise and specific.\n    snippets:"
        ));
    }
}

// =============================================================================
// Learning Notes
// =============================================================================
// • `anyhow::bail!` — immediately returns an Err from the current function
//   with a formatted message.  Saves writing `return Err(anyhow!(...))`.
// • `.with_context(|| "...")` — adds a human-readable description to any error
//   that propagates out of a `?`; the closure is only evaluated when there IS an error.
// • Raw string literals `r#"..."#` — backslashes and double-quotes are treated
//   as literal characters; great for YAML/regex/multiline template strings.
// • `io::stdout().flush()` — stdout is line-buffered by default, so print!()
//   (without a newline) may not appear until the buffer is flushed.  Always flush
//   before blocking on stdin.
// • `split_once('=')` — splits a string on the first occurrence of a delimiter,
//   returning an `Option<(&str, &str)>`.  Cleaner than split().nth() for this pattern.
