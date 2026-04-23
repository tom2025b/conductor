use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct RegenCommand {
    pub workflow_path: PathBuf,
    pub project_dir: Option<PathBuf>,
    pub force: bool,
}

#[derive(Debug, Clone)]
pub struct RegenReport {
    pub workflow_path: PathBuf,
    pub project_root: PathBuf,
    pub project_kind: &'static str,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
struct ProjectAnalysis {
    root: PathBuf,
    name: String,
    kind: ProjectKind,
    notes: Vec<String>,
    validation_commands: Vec<String>,
}

#[derive(Debug, Clone, Copy)]
enum ProjectKind {
    RustCli,
    RustLibrary,
    NodeApp,
    PythonApp,
    Generic,
}

impl ProjectKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::RustCli => "rust-cli",
            Self::RustLibrary => "rust-library",
            Self::NodeApp => "node-app",
            Self::PythonApp => "python-app",
            Self::Generic => "generic",
        }
    }
}

impl RegenCommand {
    pub fn run(self) -> Result<RegenReport> {
        let workflow_path = absolutize(&self.workflow_path)?;
        let project_root = match self.project_dir {
            Some(path) => absolutize(path)?,
            None => std::env::current_dir()?,
        };

        let analysis = analyze_project(&project_root)?;
        print_analysis(&analysis, &workflow_path);

        let custom_preferences = prompt_for_preferences()?;
        let yaml = render_workflow_yaml(&analysis, custom_preferences.as_deref());

        if workflow_path.exists() && !self.force {
            let overwrite = prompt_yes_no(
                &format!(
                    "workflow file `{}` already exists. Overwrite it?",
                    workflow_path.display()
                ),
                false,
            )?;
            if !overwrite {
                bail!("aborted without writing workflow file");
            }
        }

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

pub fn print_regen_report(report: RegenReport) {
    println!(
        "generated workflow `{}` for detected project type `{}`",
        report.workflow_path.display(),
        report.project_kind
    );
    println!("project root: {}", report.project_root.display());
    if !report.notes.is_empty() {
        for note in report.notes {
            println!("note: {note}");
        }
    }
}

fn analyze_project(project_root: &Path) -> Result<ProjectAnalysis> {
    if !project_root.is_dir() {
        bail!(
            "project directory `{}` does not exist",
            project_root.display()
        );
    }

    let cargo_toml = project_root.join("Cargo.toml");
    let package_json = project_root.join("package.json");
    let pyproject = project_root.join("pyproject.toml");
    let src_main_rs = project_root.join("src/main.rs");
    let src_lib_rs = project_root.join("src/lib.rs");

    let mut notes = Vec::new();

    let (kind, name) = if cargo_toml.exists() {
        let manifest = fs::read_to_string(&cargo_toml)
            .with_context(|| format!("failed to read `{}`", cargo_toml.display()))?;
        let package_name = parse_package_name(&manifest).unwrap_or_else(|| {
            project_root
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("project")
                .to_string()
        });
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

    if project_root.join(".git").exists() {
        notes.push("git repository detected".to_string());
    }

    let validation_commands = detect_validation_commands(project_root, kind)?;

    Ok(ProjectAnalysis {
        root: project_root.to_path_buf(),
        name,
        kind,
        notes,
        validation_commands,
    })
}

fn render_workflow_yaml(analysis: &ProjectAnalysis, custom_preferences: Option<&str>) -> String {
    let description = match analysis.kind {
        ProjectKind::RustCli => {
            "Coordinate implementation, review, and validation for a Rust CLI project."
        }
        ProjectKind::RustLibrary => {
            "Coordinate implementation, review, and validation for a Rust library project."
        }
        ProjectKind::NodeApp => {
            "Coordinate implementation, review, and validation for a Node application."
        }
        ProjectKind::PythonApp => {
            "Coordinate implementation, review, and validation for a Python project."
        }
        ProjectKind::Generic => {
            "Coordinate implementation, review, and validation for a software project."
        }
    };

    let build_commands = build_commands_for_analysis(analysis);
    let has_custom_preferences = custom_preferences
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let preferences_block = custom_preferences
        .filter(|value| !value.trim().is_empty())
        .map(render_preferences_block)
        .unwrap_or_default();
    let plan_snippets = render_step_snippets(&["project_brief"], has_custom_preferences);
    let review_snippets = render_step_snippets(&["review_brief"], has_custom_preferences);
    let implement_snippets = render_step_snippets(&[], has_custom_preferences);

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
        name = sanitize_yaml_scalar(&analysis.name),
        description = sanitize_yaml_scalar(description),
        kind = analysis.kind.as_str(),
        build_commands = build_commands,
        plan_snippets = plan_snippets,
        review_snippets = review_snippets,
        implement_snippets = implement_snippets,
        preferences_block = preferences_block,
    )
}

fn build_commands_for_analysis(analysis: &ProjectAnalysis) -> String {
    analysis
        .validation_commands
        .iter()
        .map(|command| format!("      - \"{}\"\n", sanitize_yaml_scalar(command)))
        .collect()
}

fn render_preferences_block(preferences: &str) -> String {
    let mut block = String::from("  custom_preferences:\n");
    block.push_str("    trigger: \";;prefs\"\n");
    block.push_str("    description: \"User-provided generation preferences.\"\n");
    block.push_str("    content: |\n");
    for line in preferences.lines() {
        block.push_str("      ");
        block.push_str(line);
        block.push('\n');
    }
    block
}

fn render_step_snippets(base_snippets: &[&str], include_custom_preferences: bool) -> String {
    let mut snippets: Vec<&str> = base_snippets.to_vec();
    if include_custom_preferences {
        snippets.push("custom_preferences");
    }

    if snippets.is_empty() {
        return String::new();
    }

    let mut block = String::from("    snippets:\n");
    for snippet in snippets {
        block.push_str(&format!("      - \"{snippet}\"\n"));
    }
    block
}

fn print_analysis(analysis: &ProjectAnalysis, workflow_path: &Path) {
    println!("analyzing project: {}", analysis.root.display());
    println!("detected project type: {}", analysis.kind.as_str());
    println!("workflow target: {}", workflow_path.display());
    for note in &analysis.notes {
        println!("signal: {note}");
    }
}

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
            break;
        }
        collected.push(line);
    }

    if collected.is_empty() {
        Ok(None)
    } else {
        Ok(Some(collected.join("\n")))
    }
}

fn prompt_yes_no(message: &str, default: bool) -> Result<bool> {
    let suffix = if default { "[Y/n]" } else { "[y/N]" };
    let input = prompt_line(&format!("{message} {suffix} "))?;
    let trimmed = input.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return Ok(default);
    }

    match trimmed.as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => bail!("expected yes or no"),
    }
}

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().context("failed to flush stdout")?;
    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("failed to read user input")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn absolutize(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn parse_package_name(manifest: &str) -> Option<String> {
    let mut in_package = false;
    for line in manifest.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if in_package && trimmed.starts_with("name") {
            let (_, value) = trimmed.split_once('=')?;
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn sanitize_yaml_scalar(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

fn detect_validation_commands(project_root: &Path, kind: ProjectKind) -> Result<Vec<String>> {
    let commands = match kind {
        ProjectKind::RustCli | ProjectKind::RustLibrary => detect_rust_commands(project_root)?,
        ProjectKind::NodeApp => detect_node_commands(project_root)?,
        ProjectKind::PythonApp => detect_python_commands(project_root),
        ProjectKind::Generic => vec!["echo 'add project-specific validation commands'".to_string()],
    };

    Ok(commands)
}

fn detect_rust_commands(project_root: &Path) -> Result<Vec<String>> {
    let manifest = fs::read_to_string(project_root.join("Cargo.toml")).with_context(|| {
        format!(
            "failed to read `{}`",
            project_root.join("Cargo.toml").display()
        )
    })?;

    let mut commands = vec!["cargo fmt".to_string()];
    if manifest.contains("[workspace]") {
        commands.push("cargo test --workspace".to_string());
    } else {
        commands.push("cargo test".to_string());
    }

    if manifest.contains("clippy") || project_root.join("clippy.toml").exists() {
        commands.insert(
            1,
            "cargo clippy --all-targets --all-features -- -D warnings".to_string(),
        );
    }

    Ok(commands)
}

fn detect_node_commands(project_root: &Path) -> Result<Vec<String>> {
    let package_json =
        fs::read_to_string(project_root.join("package.json")).with_context(|| {
            format!(
                "failed to read `{}`",
                project_root.join("package.json").display()
            )
        })?;

    let mut commands = Vec::new();
    if package_json.contains("\"lint\"") {
        commands.push("npm run lint".to_string());
    }
    if package_json.contains("\"test\"") {
        commands.push("npm test".to_string());
    }
    if package_json.contains("\"build\"") {
        commands.push("npm run build".to_string());
    }
    if commands.is_empty() {
        commands.push("npm test".to_string());
    }

    Ok(commands)
}

fn detect_python_commands(project_root: &Path) -> Vec<String> {
    let mut commands = Vec::new();
    if project_root.join("ruff.toml").exists() || project_root.join(".ruff.toml").exists() {
        commands.push("ruff check .".to_string());
    }
    commands.push("pytest".to_string());
    commands
}

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
