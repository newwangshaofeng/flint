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

/// The category of a build action, which determines its icon and color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildActionKind {
    Run,
    Build,
    Compile,
    Package,
    Preview,
    Install,
    Restore,
    Test,
    Lint,
}

/// Supported package managers for JavaScript/TypeScript projects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageManager {
    Npm,
    Pnpm,
    Yarn,
    Bun,
}

impl PackageManager {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
            Self::Bun => "bun",
        }
    }
}

pub fn detect_package_manager(directory: &Path, root: &Path) -> PackageManager {
    for dir in [directory, root] {
        if dir.join("pnpm-lock.yaml").exists() {
            return PackageManager::Pnpm;
        }
        if dir.join("yarn.lock").exists() {
            return PackageManager::Yarn;
        }
        if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            return PackageManager::Bun;
        }
    }
    PackageManager::Npm
}

impl BuildSystem {
    /// The system that owns `file_name`, if any. `sln`, `slnx`, and `csproj` are
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
        if extension.eq_ignore_ascii_case("sln")
            || extension.eq_ignore_ascii_case("slnx")
            || extension.eq_ignore_ascii_case("csproj")
        {
            return Some(Self::Dotnet);
        }
        None
    }

    /// The label shown for a project node when the manifest itself carries
    /// no name.
    #[allow(dead_code)]
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
    /// Action primary name, e.g. "dev", "clean package", "Build".
    pub name: SharedString,
    /// Subtitle showing the underlying command, e.g. "npm run dev / vite", "mvn clean package -DskipTests".
    pub subtitle: Option<SharedString>,
    /// Human readable label, e.g. `npm run dev` or `dotnet build App.sln`.
    pub label: SharedString,
    /// Kind of build action, which determines its icon and color.
    pub kind: BuildActionKind,
    /// Executable to spawn.
    pub program: String,
    /// Arguments to the executable.
    pub args: Vec<String>,
    /// Directory the command runs in.
    pub cwd: PathBuf,
}

impl BuildCommand {
    pub fn new(
        name: impl Into<SharedString>,
        subtitle: Option<impl Into<SharedString>>,
        label: impl Into<SharedString>,
        kind: BuildActionKind,
        program: impl Into<String>,
        args: &[&str],
        directory: &Path,
    ) -> Self {
        Self {
            name: name.into(),
            subtitle: subtitle.map(Into::into),
            label: label.into(),
            kind,
            program: program.into(),
            args: args.iter().map(|arg| arg.to_string()).collect(),
            cwd: directory.to_path_buf(),
        }
    }

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
    /// containing directory or file name.
    pub name: SharedString,
    /// Suffix for the manifest path, e.g. Some("(src/pom.xml)") or Some("(package.json)").
    pub manifest_suffix: Option<SharedString>,
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
            && self.relative_path.extension().is_some_and(|extension| {
                extension.eq_ignore_ascii_case("sln") || extension.eq_ignore_ascii_case("slnx")
            })
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

    let has_solutions = candidates.iter().any(|candidate| candidate.is_solution());
    let solution_directories: Vec<Arc<RelPath>> = candidates
        .iter()
        .filter(|candidate| candidate.is_solution())
        .map(|candidate| parent_or_root(&candidate.relative_path))
        .collect();
    candidates.retain(|candidate| {
        if candidate.system != BuildSystem::Dotnet || candidate.is_solution() {
            return true;
        }
        if has_solutions {
            return false;
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
    let pm = detect_package_manager(&directory, &candidate.root);
    let parsed = parse_manifest(candidate.system, contents, &directory, file_name, pm);
    if parsed.commands.is_empty() {
        return None;
    }
    let (name, manifest_suffix) = manifest_display_info(
        candidate.system,
        &candidate.relative_path,
        parsed.name,
        &candidate.root_name,
    );
    Some(BuildProject {
        system: candidate.system,
        name,
        manifest_suffix,
        root: candidate.root.clone(),
        directory,
        manifest_path: candidate.absolute_path(),
        relative_manifest_path: candidate.relative_path.clone(),
        commands: parsed.commands,
    })
}

fn manifest_display_info(
    system: BuildSystem,
    relative_path: &RelPath,
    parsed_name: Option<String>,
    root_name: &str,
) -> (SharedString, Option<SharedString>) {
    let file_name = relative_path.file_name().unwrap_or("");
    let rel_str = relative_path.as_unix_str();
    match system {
        BuildSystem::Dotnet => {
            let name = file_name.to_string();
            let suffix = relative_path
                .parent()
                .filter(|p| !p.is_empty())
                .map(|p| format!("({})", p.as_unix_str()));
            (name.into(), suffix.map(Into::into))
        }
        BuildSystem::Node => {
            let name = parsed_name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| directory_label(relative_path, root_name));
            let suffix = format!("({rel_str})");
            (name.into(), Some(suffix.into()))
        }
        BuildSystem::Maven => {
            let name = parsed_name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| directory_label(relative_path, root_name));
            let suffix = format!("({rel_str})");
            (name.into(), Some(suffix.into()))
        }
        BuildSystem::Go | BuildSystem::Cargo => {
            let name = parsed_name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| directory_label(relative_path, root_name));
            let suffix = format!("({rel_str})");
            (name.into(), Some(suffix.into()))
        }
    }
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
    package_manager: PackageManager,
) -> ParsedManifest {
    match system {
        BuildSystem::Node => parse_node(contents, directory, package_manager),
        BuildSystem::Maven => parse_maven(contents, directory),
        BuildSystem::Go => parse_go(contents, directory),
        BuildSystem::Dotnet => parse_dotnet(directory, manifest_file_name),
        BuildSystem::Cargo => parse_cargo(contents, directory),
    }
}

fn parse_node(contents: &str, directory: &Path, package_manager: PackageManager) -> ParsedManifest {
    let mut parsed = ParsedManifest::default();
    let Ok(json) = serde_json::from_str::<serde_json::Value>(contents) else {
        return parsed;
    };
    parsed.name = json
        .get("name")
        .and_then(|name| name.as_str())
        .map(str::to_string);
    let pm_str = package_manager.as_str();
    let mut has_install = false;
    if let Some(scripts) = json.get("scripts").and_then(|scripts| scripts.as_object()) {
        for (script_name, script) in scripts {
            if !script.is_string() {
                continue;
            }
            if script_name == "install" {
                has_install = true;
            }
            let script_val = script.as_str().unwrap_or("");
            let kind = match script_name.as_str() {
                "dev" | "start" | "serve" => BuildActionKind::Run,
                "build" => BuildActionKind::Build,
                "preview" => BuildActionKind::Preview,
                s if s.contains("test") => BuildActionKind::Test,
                s if s.contains("lint") || s.contains("check") => BuildActionKind::Lint,
                _ => BuildActionKind::Run,
            };
            let subtitle = if !script_val.trim().is_empty() && script_val.trim() != script_name {
                let trimmed = script_val.trim();
                let display_script = if trimmed.len() > 40 {
                    format!("{}…", &trimmed[..38])
                } else {
                    trimmed.to_string()
                };
                format!("{pm_str} run {script_name} / {display_script}")
            } else {
                format!("{pm_str} run {script_name}")
            };
            parsed.commands.push(BuildCommand::new(
                script_name.as_str(),
                Some(subtitle),
                format!("{pm_str} run {script_name}"),
                kind,
                pm_str,
                &["run", script_name],
                directory,
            ));
        }
    }
    if !has_install {
        parsed.commands.push(BuildCommand::new(
            "install",
            Some(format!("{pm_str} install")),
            format!("{pm_str} install"),
            BuildActionKind::Install,
            pm_str,
            &["install"],
            directory,
        ));
    }
    parsed
}

static MAVEN_ARTIFACT_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<artifactId>\s*([^<\s]+)\s*</artifactId>").expect("valid regex"));

fn parse_maven(contents: &str, directory: &Path) -> ParsedManifest {
    let mut parsed = ParsedManifest::default();
    parsed.name = MAVEN_ARTIFACT_ID
        .captures(contents)
        .and_then(|captures| captures.get(1))
        .map(|artifact_id| artifact_id.as_str().to_string());
    parsed.commands = vec![
        BuildCommand::new(
            "clean compile",
            Some("mvn clean compile"),
            "mvn clean compile",
            BuildActionKind::Compile,
            "mvn",
            &["clean", "compile"],
            directory,
        ),
        BuildCommand::new(
            "clean package",
            Some("mvn clean package -DskipTests"),
            "mvn clean package",
            BuildActionKind::Package,
            "mvn",
            &["clean", "package", "-DskipTests"],
            directory,
        ),
        BuildCommand::new(
            "clean install",
            Some("mvn clean install"),
            "mvn clean install",
            BuildActionKind::Install,
            "mvn",
            &["clean", "install"],
            directory,
        ),
        BuildCommand::new(
            "test",
            Some("mvn test"),
            "mvn test",
            BuildActionKind::Test,
            "mvn",
            &["test"],
            directory,
        ),
    ];
    parsed
}

fn parse_go(contents: &str, directory: &Path) -> ParsedManifest {
    let mut parsed = ParsedManifest::default();
    parsed.name = contents.lines().find_map(|line| {
        line.trim()
            .strip_prefix("module ")
            .map(str::trim)
            .filter(|module| !module.is_empty())
            .map(str::to_string)
    });
    parsed.commands = vec![
        BuildCommand::new(
            "build",
            Some("go build ./..."),
            "go build ./...",
            BuildActionKind::Build,
            "go",
            &["build", "./..."],
            directory,
        ),
        BuildCommand::new(
            "test",
            Some("go test ./..."),
            "go test ./...",
            BuildActionKind::Test,
            "go",
            &["test", "./..."],
            directory,
        ),
        BuildCommand::new(
            "vet",
            Some("go vet ./..."),
            "go vet ./...",
            BuildActionKind::Lint,
            "go",
            &["vet", "./..."],
            directory,
        ),
        BuildCommand::new(
            "fmt",
            Some("go fmt ./..."),
            "go fmt ./...",
            BuildActionKind::Lint,
            "go",
            &["fmt", "./..."],
            directory,
        ),
    ];
    parsed
}

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
    parsed.commands = vec![
        BuildCommand::new(
            "build",
            Some("cargo build"),
            "cargo build",
            BuildActionKind::Build,
            "cargo",
            &["build"],
            directory,
        ),
        BuildCommand::new(
            "run",
            Some("cargo run"),
            "cargo run",
            BuildActionKind::Run,
            "cargo",
            &["run"],
            directory,
        ),
        BuildCommand::new(
            "test",
            Some("cargo test"),
            "cargo test",
            BuildActionKind::Test,
            "cargo",
            &["test"],
            directory,
        ),
        BuildCommand::new(
            "check",
            Some("cargo check"),
            "cargo check",
            BuildActionKind::Lint,
            "cargo",
            &["check"],
            directory,
        ),
        BuildCommand::new(
            "clippy",
            Some("cargo clippy"),
            "cargo clippy",
            BuildActionKind::Lint,
            "cargo",
            &["clippy"],
            directory,
        ),
        BuildCommand::new(
            "fmt",
            Some("cargo fmt"),
            "cargo fmt",
            BuildActionKind::Lint,
            "cargo",
            &["fmt"],
            directory,
        ),
    ];
    parsed
}

fn parse_dotnet(directory: &Path, manifest_file_name: &str) -> ParsedManifest {
    let is_solution = manifest_file_name.to_ascii_lowercase().ends_with(".sln")
        || manifest_file_name.to_ascii_lowercase().ends_with(".slnx");
    let mut commands = vec![
        BuildCommand::new(
            "Restore",
            Some(format!("dotnet restore {manifest_file_name}")),
            "dotnet restore",
            BuildActionKind::Restore,
            "dotnet",
            &["restore", manifest_file_name],
            directory,
        ),
        BuildCommand::new(
            "Build",
            Some(format!("dotnet build {manifest_file_name}")),
            format!("dotnet build {manifest_file_name}"),
            BuildActionKind::Build,
            "dotnet",
            &["build", manifest_file_name],
            directory,
        ),
        BuildCommand::new(
            "Test",
            Some(format!("dotnet test {manifest_file_name}")),
            format!("dotnet test {manifest_file_name}"),
            BuildActionKind::Test,
            "dotnet",
            &["test", manifest_file_name],
            directory,
        ),
    ];
    if !is_solution {
        commands.push(BuildCommand::new(
            "Run",
            Some(format!("dotnet run --project {manifest_file_name}")),
            format!("dotnet run --project {manifest_file_name}"),
            BuildActionKind::Run,
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
        parse_manifest(
            system,
            contents,
            Path::new("/work/project"),
            file_name,
            PackageManager::Npm,
        )
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
            manifest_suffix: None,
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
            BuildSystem::from_manifest_file_name("App.slnx"),
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
                "npm install".to_string(),
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
        assert_eq!(lines.first().map(String::as_str), Some("mvn clean compile"));
        assert!(lines.contains(&"mvn clean package -DskipTests".to_string()));
        assert!(lines.contains(&"mvn clean install".to_string()));
        assert!(lines.contains(&"mvn test".to_string()));
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
        assert!(lines.contains(&"go test ./...".to_string()));
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
        assert_eq!(project.name.as_str(), "Api.csproj");
        assert_eq!(project.manifest_suffix.as_deref(), Some("(services/api)"));
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
        assert_eq!(
            project.manifest_suffix.as_deref(),
            Some("(services/api/go.mod)")
        );
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
        assert_eq!(project.manifest_suffix.as_deref(), Some("(package.json)"));
        assert_eq!(project.directory, Path::new("/work/web"));
    }

    #[test]
    fn task_template_disambiguates_identical_script_names_across_projects() {
        let first = project(BuildSystem::Node, "web", "/work/web", "package.json");
        let second = project(BuildSystem::Node, "admin", "/work/admin", "package.json");
        let command = BuildCommand::new(
            "build",
            Some("npm run build"),
            "npm run build",
            BuildActionKind::Build,
            "npm",
            &["run", "build"],
            Path::new("/work/web"),
        );

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
        let command = BuildCommand::new(
            "build",
            Some("go build ./..."),
            "go build",
            BuildActionKind::Build,
            "go",
            &["build"],
            Path::new("/work/services/api"),
        );

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
