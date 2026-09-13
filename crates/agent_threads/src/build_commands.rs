//! Auto-detection of build tooling for the Agent Threads panel's "Build"
//! section.
//!
//! This module owns everything that can be decided from a manifest file
//! alone: which build systems a directory contains, what the manifest is
//! named, and which commands the manifest implies. The panel owns the I/O
//! and the UI, so this stays testable without a window or a filesystem.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;

use anyhow::{Context as _, Result};
use fs::Fs;
use gpui::{App, SharedString};
use project::Project;
use regex::Regex;
use task::{TaskContext, TaskTemplate, TaskVariables, VariableName};
use util::rel_path::RelPath;

/// Directories that never hold a project the user wants a shortcut for,
/// even when they happen to contain a manifest file. Normally these are
/// already excluded by gitignore, but a repository that commits its
/// dependencies would otherwise flood the section with one node per
/// package.
const SKIPPED_DIRECTORIES: &[&str] = &[
    "node_modules",
    "target",
    "vendor",
    "obj",
    "dist",
    "__pycache__",
];

/// A build tool Flint recognizes from a manifest file name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BuildSystem {
    Node,
    Maven,
    Go,
    Dotnet,
    Cargo,
}

impl BuildSystem {
    /// The system that owns `file_name`, if any. `sln` and `csproj` are
    /// matched by extension because their stems are project names.
    pub fn from_manifest_file_name(file_name: &str) -> Option<Self> {
        if file_name.eq_ignore_ascii_case("package.json") {
            return Some(Self::Node);
        }
        if file_name.eq_ignore_ascii_case("pom.xml") {
            return Some(Self::Maven);
        }
        if file_name.eq_ignore_ascii_case("go.mod") {
            return Some(Self::Go);
        }
        if file_name.eq_ignore_ascii_case("Cargo.toml") {
            return Some(Self::Cargo);
        }
        let extension = Path::new(file_name).extension()?.to_str()?;
        if extension.eq_ignore_ascii_case("sln") || extension.eq_ignore_ascii_case("csproj") {
            return Some(Self::Dotnet);
        }
        None
    }

    /// The label shown for a project node when the manifest itself carries
    /// no name.
    pub fn label(self) -> &'static str {
        match self {
            Self::Node => "npm",
            Self::Maven => "Maven",
            Self::Go => "Go",
            Self::Dotnet => ".NET",
            Self::Cargo => "Cargo",
        }
    }
}

/// One runnable command derived from a manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildCommand {
    /// Human readable label, e.g. `npm run dev`.
    pub label: SharedString,
    /// Executable to spawn.
    pub program: String,
    /// Arguments to the executable.
    pub args: Vec<String>,
    /// Directory the command runs in.
    pub cwd: PathBuf,
}

impl BuildCommand {
    /// Builds the task template used to spawn this command.
    ///
    /// `project_label` is folded into the task label because the terminal
    /// reuses an existing tab by label: two projects that both define
    /// `npm run build` must not share one terminal.
    pub fn task_template(&self, project_label: &str) -> TaskTemplate {
        TaskTemplate {
            label: format!("{} — {}", self.label, project_label),
            command: self.program.clone(),
            args: self.args.clone(),
            cwd: Some(self.cwd.to_string_lossy().into_owned()),
            ..TaskTemplate::default()
        }
    }
}

/// A detected project: one manifest file and the commands it implies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuildProject {
    pub system: BuildSystem,
    /// Display name, from the manifest when it carries one, else the
    /// containing directory.
    pub name: SharedString,
    /// Absolute path of the worktree root this project belongs to.
    pub root: PathBuf,
    /// Absolute path of the directory containing the manifest.
    pub directory: PathBuf,
    /// Absolute path of the manifest file.
    pub manifest_path: PathBuf,
    /// Path of the manifest relative to its worktree root. Kept alongside the
    /// absolute path because worktree change events report relative paths.
    pub relative_manifest_path: Arc<RelPath>,
    pub commands: Vec<BuildCommand>,
}

impl BuildProject {
    /// The task context for a command of this project, carrying the
    /// worktree root so task variables referencing it resolve.
    pub fn task_context(&self, command: &BuildCommand) -> TaskContext {
        let mut task_variables = TaskVariables::default();
        task_variables.insert(
            VariableName::WorktreeRoot,
            self.root.to_string_lossy().into_owned(),
        );
        TaskContext {
            cwd: Some(command.cwd.clone()),
            task_variables,
            project_env: Default::default(),
        }
    }
}

/// A manifest file found in a visible worktree, before it is read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestCandidate {
    pub system: BuildSystem,
    /// Absolute path of the worktree root.
    pub root: PathBuf,
    /// Name of the worktree root directory, used as a display fallback.
    pub root_name: String,
    /// Path of the manifest relative to the worktree root.
    pub relative_path: Arc<RelPath>,
}

impl ManifestCandidate {
    pub fn absolute_path(&self) -> PathBuf {
        self.root.join(self.relative_path.as_std_path())
    }

    pub fn directory(&self) -> PathBuf {
        self.root.join(
            self.relative_path
                .parent()
                .unwrap_or(RelPath::empty())
                .as_std_path(),
        )
    }

    fn is_solution(&self) -> bool {
        self.system == BuildSystem::Dotnet
            && self
                .relative_path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("sln"))
    }
}

/// Collects every manifest file in the project's visible worktrees.
///
/// Only entries already present in the worktree snapshot are considered,
/// which keeps this off the filesystem entirely and inherits the project's
/// ignore rules. `.NET` project files covered by a sibling solution are
/// dropped so a solution and its projects don't each get a node.
pub fn collect_candidates(project: &Project, cx: &App) -> Vec<ManifestCandidate> {
    let mut candidates = Vec::new();
    for worktree in project.visible_worktrees(cx) {
        let worktree = worktree.read(cx);
        let root = worktree.abs_path().to_path_buf();
        let root_name = worktree.snapshot().root_name_str().to_string();
        for entry in worktree.snapshot().entries(false, 0) {
            if !entry.kind.is_file() {
                continue;
            }
            let Some(file_name) = entry.path.file_name() else {
                continue;
            };
            let Some(system) = BuildSystem::from_manifest_file_name(file_name) else {
                continue;
            };
            if has_skipped_ancestor(&entry.path) {
                continue;
            }
            candidates.push(ManifestCandidate {
                system,
                root: root.clone(),
                root_name: root_name.clone(),
                relative_path: entry.path.clone(),
            });
        }
    }

    let solution_directories: Vec<Arc<RelPath>> = candidates
        .iter()
        .filter(|candidate| candidate.is_solution())
        .map(|candidate| parent_or_root(&candidate.relative_path))
        .collect();
    candidates.retain(|candidate| {
        if candidate.system != BuildSystem::Dotnet || candidate.is_solution() {
            return true;
        }
        let parent = parent_or_root(&candidate.relative_path);
        !solution_directories
            .iter()
            .any(|solution| parent.starts_with(solution))
    });

    candidates.sort_by(|left, right| {
        left.root
            .cmp(&right.root)
            .then_with(|| left.relative_path.cmp(&right.relative_path))
    });
    candidates
}

fn parent_or_root(path: &RelPath) -> Arc<RelPath> {
    path.parent()
        .map(Arc::from)
        .unwrap_or_else(|| RelPath::empty().into())
}

fn has_skipped_ancestor(path: &RelPath) -> bool {
    path.parent().is_some_and(|parent| {
        parent.ancestors().any(|ancestor| {
            ancestor.file_name().is_some_and(|name| {
                SKIPPED_DIRECTORIES
                    .iter()
                    .any(|skip| name.eq_ignore_ascii_case(skip))
            })
        })
    })
}

/// The result of reading one manifest file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ParsedManifest {
    /// Name declared by the manifest, if any.
    pub name: Option<String>,
    pub commands: Vec<BuildCommand>,
}

/// Turns `candidate` plus the manifest's contents into a project, or `None`
/// when the manifest yields no commands.
pub fn build_project(candidate: &ManifestCandidate, contents: &str) -> Option<BuildProject> {
    let file_name = candidate.relative_path.file_name()?;
    let directory = candidate.directory();
    let parsed = parse_manifest(candidate.system, contents, &directory, file_name);
    if parsed.commands.is_empty() {
        return None;
    }
    let name = parsed
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| directory_label(&candidate.relative_path, &candidate.root_name));
    Some(BuildProject {
        system: candidate.system,
        name: name.into(),
        root: candidate.root.clone(),
        directory,
        manifest_path: candidate.absolute_path(),
        relative_manifest_path: candidate.relative_path.clone(),
        commands: parsed.commands,
    })
}

/// The display name for a project whose manifest declares none: the path
/// relative to the worktree root, or the root's own name at the top level.
fn directory_label(relative_path: &RelPath, root_name: &str) -> String {
    match relative_path.parent() {
        Some(parent) if !parent.is_empty() => parent.as_unix_str().to_string(),
        _ => root_name.to_string(),
    }
}

/// Reads a manifest file, either from the local filesystem or from the
/// remote host when the project is remote.
pub async fn load_manifest(
    fs: &Arc<dyn Fs>,
    remote_client: Option<&rpc::AnyProtoClient>,
    absolute_path: &Path,
) -> Result<String> {
    match remote_client {
        Some(proto_client) => {
            let response = proto_client
                .request(proto::ReadRemoteFile {
                    dev_server_id: proto::REMOTE_SERVER_PROJECT_ID,
                    path: absolute_path.to_string_lossy().into_owned(),
                })
                .await?;
            String::from_utf8(response.content).context("manifest is not valid UTF-8")
        }
        None => fs
            .load(absolute_path)
            .await
            .with_context(|| format!("reading manifest {}", absolute_path.display())),
    }
}

pub fn parse_manifest(
    system: BuildSystem,
    contents: &str,
    directory: &Path,
    manifest_file_name: &str,
) -> ParsedManifest {
    match system {
        BuildSystem::Node => parse_node(contents, directory),
        BuildSystem::Maven => parse_maven(contents, directory),
        BuildSystem::Go => parse_go(contents, directory),
        BuildSystem::Dotnet => parse_dotnet(directory, manifest_file_name),
        BuildSystem::Cargo => parse_cargo(contents, directory),
    }
}

fn command(label: String, program: &str, args: &[&str], directory: &Path) -> BuildCommand {
    BuildCommand {
        label: label.into(),
        program: program.to_string(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        cwd: directory.to_path_buf(),
    }
}

fn parse_node(contents: &str, directory: &Path) -> ParsedManifest {
    let mut parsed = ParsedManifest::default();
    let Ok(json) = serde_json::from_str::<serde_json::Value>(contents) else {
        return parsed;
    };
    parsed.name = json
        .get("name")
        .and_then(|name| name.as_str())
        .map(str::to_string);
    if let Some(scripts) = json.get("scripts").and_then(|scripts| scripts.as_object()) {
        for (script_name, script) in scripts {
            // `serde_json` preserves object order, so scripts keep the order
            // they were written in rather than being sorted.
            if !script.is_string() {
                continue;
            }
            parsed.commands.push(command(
                format!("npm run {script_name}"),
                "npm",
                &["run", script_name],
                directory,
            ));
        }
    }
    if parsed.commands.is_empty() {
        parsed.commands.push(command(
            "npm install".to_string(),
            "npm",
            &["install"],
            directory,
        ));
    }
    parsed
}

/// The standard Maven lifecycle, in the order Maven defines it.
const MAVEN_LIFECYCLE: &[&str] = &[
    "clean", "validate", "compile", "test", "package", "verify", "install", "deploy",
];

static MAVEN_ARTIFACT_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<artifactId>\s*([^<\s]+)\s*</artifactId>").expect("valid regex"));

fn parse_maven(contents: &str, directory: &Path) -> ParsedManifest {
    let mut parsed = ParsedManifest::default();
    parsed.name = MAVEN_ARTIFACT_ID
        .captures(contents)
        .and_then(|captures| captures.get(1))
        .map(|artifact_id| artifact_id.as_str().to_string());
    parsed.commands = MAVEN_LIFECYCLE
        .iter()
        .map(|phase| command(format!("mvn {phase}"), "mvn", &[phase], directory))
        .collect();
    parsed
}

/// The Go commands worth a shortcut. `./...` covers every package in the
/// module so a multi-package module needs no per-package entries.
const GO_COMMANDS: &[(&str, &[&str])] = &[
    ("build", &["build", "./..."]),
    ("test", &["test", "./..."]),
    ("vet", &["vet", "./..."]),
    ("fmt", &["fmt", "./..."]),
];

fn parse_go(contents: &str, directory: &Path) -> ParsedManifest {
    let mut parsed = ParsedManifest::default();
    parsed.name = contents.lines().find_map(|line| {
        line.trim()
            .strip_prefix("module ")
            .map(str::trim)
            .filter(|module| !module.is_empty())
            .map(str::to_string)
    });
    parsed.commands = GO_COMMANDS
        .iter()
        .map(|(name, args)| command(format!("go {name}"), "go", args, directory))
        .collect();
    parsed
}

const CARGO_COMMANDS: &[(&str, &[&str])] = &[
    ("build", &["build"]),
    ("run", &["run"]),
    ("test", &["test"]),
    ("check", &["check"]),
    ("clippy", &["clippy"]),
    ("fmt", &["fmt"]),
];

fn parse_cargo(contents: &str, directory: &Path) -> ParsedManifest {
    let mut parsed = ParsedManifest::default();
    parsed.name = toml::from_str::<toml::Value>(contents)
        .ok()
        .and_then(|manifest| {
            manifest
                .get("package")?
                .get("name")?
                .as_str()
                .map(str::to_string)
        });
    parsed.commands = CARGO_COMMANDS
        .iter()
        .map(|(name, args)| command(format!("cargo {name}"), "cargo", args, directory))
        .collect();
    parsed
}

fn parse_dotnet(directory: &Path, manifest_file_name: &str) -> ParsedManifest {
    let is_solution = manifest_file_name.to_ascii_lowercase().ends_with(".sln");
    let mut commands = vec![
        command(
            "dotnet restore".to_string(),
            "dotnet",
            &["restore", manifest_file_name],
            directory,
        ),
        command(
            "dotnet build".to_string(),
            "dotnet",
            &["build", manifest_file_name],
            directory,
        ),
        command(
            "dotnet test".to_string(),
            "dotnet",
            &["test", manifest_file_name],
            directory,
        ),
    ];
    // `dotnet run` needs a project, not a solution.
    if !is_solution {
        commands.push(command(
            "dotnet run".to_string(),
            "dotnet",
            &["run", "--project", manifest_file_name],
            directory,
        ));
    }
    ParsedManifest {
        name: None,
        commands,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(system: BuildSystem, contents: &str, file_name: &str) -> ParsedManifest {
        parse_manifest(system, contents, Path::new("/work/project"), file_name)
    }

    fn command_lines(parsed: &ParsedManifest) -> Vec<String> {
        parsed
            .commands
            .iter()
            .map(|command| {
                std::iter::once(command.program.clone())
                    .chain(command.args.iter().cloned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect()
    }

    fn project(
        system: BuildSystem,
        name: &str,
        root: &str,
        relative_manifest: &str,
    ) -> BuildProject {
        let relative_manifest_path: Arc<RelPath> = RelPath::unix(relative_manifest).unwrap().into();
        let directory = Path::new(root).join(
            relative_manifest_path
                .parent()
                .unwrap_or(RelPath::empty())
                .as_std_path(),
        );
        BuildProject {
            system,
            name: name.into(),
            root: PathBuf::from(root),
            directory,
            manifest_path: Path::new(root).join(relative_manifest_path.as_std_path()),
            relative_manifest_path,
            commands: vec![],
        }
    }

    #[test]
    fn manifest_file_names_map_to_their_build_system() {
        assert_eq!(
            BuildSystem::from_manifest_file_name("package.json"),
            Some(BuildSystem::Node)
        );
        assert_eq!(
            BuildSystem::from_manifest_file_name("pom.xml"),
            Some(BuildSystem::Maven)
        );
        assert_eq!(
            BuildSystem::from_manifest_file_name("go.mod"),
            Some(BuildSystem::Go)
        );
        assert_eq!(
            BuildSystem::from_manifest_file_name("Cargo.toml"),
            Some(BuildSystem::Cargo)
        );
        assert_eq!(
            BuildSystem::from_manifest_file_name("App.sln"),
            Some(BuildSystem::Dotnet)
        );
        assert_eq!(
            BuildSystem::from_manifest_file_name("App.csproj"),
            Some(BuildSystem::Dotnet)
        );
        assert_eq!(BuildSystem::from_manifest_file_name("README.md"), None);
        assert_eq!(BuildSystem::from_manifest_file_name("tsconfig.json"), None);
    }

    #[test]
    fn node_scripts_keep_their_definition_order() {
        let parsed = parse(
            BuildSystem::Node,
            r#"{
                "name": "web-app",
                "scripts": {
                    "dev": "vite",
                    "build": "vite build",
                    "test": "vitest"
                }
            }"#,
            "package.json",
        );

        assert_eq!(parsed.name.as_deref(), Some("web-app"));
        assert_eq!(
            command_lines(&parsed),
            vec![
                "npm run dev".to_string(),
                "npm run build".to_string(),
                "npm run test".to_string(),
            ]
        );
    }

    #[test]
    fn node_without_scripts_falls_back_to_install() {
        let parsed = parse(
            BuildSystem::Node,
            r#"{ "name": "lib", "dependencies": {} }"#,
            "package.json",
        );
        assert_eq!(command_lines(&parsed), vec!["npm install".to_string()]);
    }

    #[test]
    fn node_with_invalid_json_yields_no_commands() {
        let parsed = parse(BuildSystem::Node, "{ not json", "package.json");
        assert!(parsed.commands.is_empty());
    }

    #[test]
    fn maven_offers_the_standard_lifecycle() {
        let parsed = parse(
            BuildSystem::Maven,
            r#"<project>
                 <artifactId>demo-service</artifactId>
               </project>"#,
            "pom.xml",
        );

        assert_eq!(parsed.name.as_deref(), Some("demo-service"));
        let lines = command_lines(&parsed);
        assert_eq!(lines.first().map(String::as_str), Some("mvn clean"));
        assert!(lines.contains(&"mvn test".to_string()));
        assert!(lines.contains(&"mvn package".to_string()));
    }

    #[test]
    fn go_commands_cover_every_package_in_the_module() {
        let parsed = parse(
            BuildSystem::Go,
            "module example.com/service\n\ngo 1.22\n",
            "go.mod",
        );

        assert_eq!(parsed.name.as_deref(), Some("example.com/service"));
        let lines = command_lines(&parsed);
        assert!(lines.contains(&"go build ./...".to_string()));
        assert!(lines.contains(&"go fmt ./...".to_string()));
    }

    #[test]
    fn cargo_reads_the_package_name_and_offers_core_commands() {
        let parsed = parse(
            BuildSystem::Cargo,
            "[package]\nname = \"flint-widget\"\nversion = \"0.1.0\"\n",
            "Cargo.toml",
        );

        assert_eq!(parsed.name.as_deref(), Some("flint-widget"));
        let lines = command_lines(&parsed);
        assert!(lines.contains(&"cargo build".to_string()));
        assert!(lines.contains(&"cargo test".to_string()));
    }

    #[test]
    fn dotnet_solution_skips_run_which_needs_a_project() {
        let parsed = parse(BuildSystem::Dotnet, "", "App.sln");
        let lines = command_lines(&parsed);
        assert!(lines.contains(&"dotnet build App.sln".to_string()));
        assert!(
            !lines.iter().any(|line| line.starts_with("dotnet run")),
            "a solution has no runnable project: {lines:?}"
        );
    }

    #[test]
    fn dotnet_project_offers_run() {
        let parsed = parse(BuildSystem::Dotnet, "", "App.csproj");
        let lines = command_lines(&parsed);
        assert!(lines.contains(&"dotnet run --project App.csproj".to_string()));
    }

    #[test]
    fn build_project_names_fall_back_to_the_containing_directory() {
        // A `.csproj` carries no name of its own, so the containing directory
        // is the only useful label.
        let candidate = ManifestCandidate {
            system: BuildSystem::Dotnet,
            root: PathBuf::from("/work"),
            root_name: "work".to_string(),
            relative_path: RelPath::unix("services/api/Api.csproj").unwrap().into(),
        };
        let project = build_project(&candidate, "").expect("dotnet project");
        assert_eq!(project.name.as_str(), "services/api");
        assert_eq!(project.directory, Path::new("/work/services/api"));
        assert_eq!(
            project.manifest_path,
            Path::new("/work/services/api/Api.csproj")
        );
    }

    #[test]
    fn build_project_prefers_the_name_declared_by_the_manifest() {
        let candidate = ManifestCandidate {
            system: BuildSystem::Go,
            root: PathBuf::from("/work"),
            root_name: "work".to_string(),
            relative_path: RelPath::unix("services/api/go.mod").unwrap().into(),
        };
        let project = build_project(&candidate, "module example.com/api\n").expect("go project");
        assert_eq!(project.name.as_str(), "example.com/api");
    }

    #[test]
    fn build_project_at_the_root_uses_the_worktree_name() {
        let candidate = ManifestCandidate {
            system: BuildSystem::Node,
            root: PathBuf::from("/work/web"),
            root_name: "web".to_string(),
            relative_path: RelPath::unix("package.json").unwrap().into(),
        };
        let project = build_project(&candidate, r#"{ "scripts": { "build": "vite build" } }"#)
            .expect("node project");
        assert_eq!(project.name.as_str(), "web");
        assert_eq!(project.directory, Path::new("/work/web"));
    }

    #[test]
    fn task_template_disambiguates_identical_script_names_across_projects() {
        let first = project(BuildSystem::Node, "web", "/work/web", "package.json");
        let second = project(BuildSystem::Node, "admin", "/work/admin", "package.json");
        let command = BuildCommand {
            label: "npm run build".into(),
            program: "npm".to_string(),
            args: vec!["run".to_string(), "build".to_string()],
            cwd: PathBuf::from("/work/web"),
        };

        let first_template = command.task_template(&first.name);
        let second_template = command.task_template(&second.name);

        assert_ne!(first_template.label, second_template.label);
        assert!(first_template.label.contains("npm run build"));
        assert!(first_template.label.contains("web"));
        assert_eq!(first_template.cwd.as_deref(), Some("/work/web"));
    }

    #[test]
    fn task_context_points_at_the_command_directory() {
        let project = project(BuildSystem::Go, "api", "/work", "services/api/go.mod");
        let command = BuildCommand {
            label: "go build".into(),
            program: "go".to_string(),
            args: vec!["build".to_string()],
            cwd: PathBuf::from("/work/services/api"),
        };

        let context = project.task_context(&command);
        assert_eq!(context.cwd, Some(PathBuf::from("/work/services/api")));
        assert_eq!(
            context.task_variables.get(&VariableName::WorktreeRoot),
            Some("/work")
        );
    }

    #[test]
    fn skipped_directories_are_detected_at_any_depth() {
        assert!(has_skipped_ancestor(
            RelPath::unix("node_modules/left-pad/package.json").unwrap()
        ));
        assert!(has_skipped_ancestor(
            RelPath::unix("crates/agent/target/debug/Cargo.toml").unwrap()
        ));
        assert!(!has_skipped_ancestor(
            RelPath::unix("web/package.json").unwrap()
        ));
        assert!(!has_skipped_ancestor(
            RelPath::unix("package.json").unwrap()
        ));
    }
}
