//! Publishes this instance to `~/.factory/ide/<port>.lock` so Factory's CLI
//! can find the bridge without any user configuration.
//!
//! The lock file is the whole discovery contract: the CLI watches that folder
//! and matches the current directory against `workspaceFolders`.

use anyhow::{Context as _, Result};
use serde_json::json;
use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Name reported to the CLI. Also what the CLI shows when several IDEs are open.
pub const IDE_NAME: &str = "Flint";

pub fn lock_dir() -> PathBuf {
    util::paths::home_dir().join(".factory").join("ide")
}

pub fn lock_path(port: u16) -> PathBuf {
    lock_dir().join(format!("{port}.lock"))
}

/// Owns the lock file for one running bridge.
pub struct LockFile {
    path: PathBuf,
    pid: u32,
    workspace_folders: BTreeSet<PathBuf>,
}

impl LockFile {
    pub fn new(port: u16) -> Self {
        Self {
            path: lock_path(port),
            pid: std::process::id(),
            workspace_folders: BTreeSet::new(),
        }
    }

    /// Overrides the lock path. Test-only entry point so tests never touch the
    /// real user directory.
    #[cfg(test)]
    pub fn at_path(path: PathBuf, pid: u32) -> Self {
        Self {
            path,
            pid,
            workspace_folders: BTreeSet::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn workspace_folders(&self) -> Vec<PathBuf> {
        self.workspace_folders.iter().cloned().collect()
    }

    /// Records a workspace root and rewrites the lock file if that changed
    /// anything.
    ///
    /// Only top-most roots are kept. Nested entries make the CLI, which matches
    /// its working directory against this list, discover the same server
    /// several times and register its tools twice.
    pub fn add_workspace_folder(&mut self, folder: &Path) -> Result<bool> {
        let Some(folder) = normalize(folder) else {
            return Ok(false);
        };

        if self
            .workspace_folders
            .iter()
            .any(|existing| is_same_or_descendant(&folder, existing))
        {
            return Ok(false);
        }

        self.workspace_folders
            .retain(|existing| !is_same_or_descendant(existing, &folder));
        self.workspace_folders.insert(folder);
        self.write()?;
        Ok(true)
    }

    pub fn remove_workspace_folder(&mut self, folder: &Path) -> Result<bool> {
        let Some(folder) = normalize(folder) else {
            return Ok(false);
        };

        if !self.workspace_folders.remove(&folder) {
            return Ok(false);
        }

        self.write()?;
        Ok(true)
    }

    pub fn replace_workspace_folders(
        &mut self,
        folders: impl IntoIterator<Item = PathBuf>,
    ) -> Result<bool> {
        let mut next: BTreeSet<PathBuf> = BTreeSet::new();
        for folder in folders {
            let Some(folder) = normalize(&folder) else {
                continue;
            };
            if next
                .iter()
                .any(|existing| is_same_or_descendant(&folder, existing))
            {
                continue;
            }
            next.retain(|existing| !is_same_or_descendant(existing, &folder));
            next.insert(folder);
        }

        if next == self.workspace_folders {
            return Ok(false);
        }

        self.workspace_folders = next;
        self.write()?;
        Ok(true)
    }

    pub fn write(&self) -> Result<()> {
        let directory = self
            .path
            .parent()
            .context("lock file has no parent directory")?;
        std::fs::create_dir_all(directory)
            .with_context(|| format!("failed to create {directory:?}"))?;

        let payload = json!({
            "pid": self.pid,
            "ideName": IDE_NAME,
            "workspaceFolders": self
                .workspace_folders
                .iter()
                .map(|folder| folder.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
        });

        // Write through a temporary file so a reader never observes a partial
        // document.
        let temporary = self.path.with_extension("lock.tmp");
        {
            let mut file = std::fs::File::create(&temporary)
                .with_context(|| format!("failed to create {temporary:?}"))?;
            file.write_all(payload.to_string().as_bytes())
                .with_context(|| format!("failed to write {temporary:?}"))?;
            file.sync_all()
                .with_context(|| format!("failed to flush {temporary:?}"))?;
        }
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("failed to publish {:?}", self.path))?;
        Ok(())
    }

    /// Removes the lock file. Safe to call when it was never written.
    pub fn remove(&self) {
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            log::warn!(
                "droid_mcp: failed to remove lock file {:?}: {error}",
                self.path
            );
        }
    }
}

/// Absolute, separator-trimmed form of `folder`, or `None` if it has no
/// meaningful path left (for example a bare root).
fn normalize(folder: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(folder).ok()?;
    let trimmed = absolute
        .to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_string();
    if trimmed.is_empty() {
        return None;
    }
    Some(PathBuf::from(trimmed))
}

fn is_same_or_descendant(candidate: &Path, ancestor: &Path) -> bool {
    if candidate == ancestor {
        return true;
    }
    candidate.starts_with(ancestor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_lock() -> (tempfile::TempDir, LockFile) {
        let directory = tempfile::tempdir().expect("temp dir");
        let lock = LockFile::at_path(directory.path().join("1234.lock"), 4321);
        (directory, lock)
    }

    /// `normalize` resolves against the current directory, so expectations must
    /// be built the same way rather than hard-coded as POSIX paths.
    fn normalized(path: &str) -> PathBuf {
        normalize(Path::new(path)).expect("normalizable path")
    }

    #[test]
    fn writes_the_documented_payload() {
        let (_directory, mut lock) = temp_lock();
        lock.add_workspace_folder(Path::new("/work/project"))
            .expect("add folder");

        let contents = std::fs::read_to_string(lock.path()).expect("read lock file");
        let payload: serde_json::Value = serde_json::from_str(&contents).expect("parse lock file");
        assert_eq!(payload["pid"], 4321);
        assert_eq!(payload["ideName"], "Flint");
        assert_eq!(payload["workspaceFolders"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn nested_roots_collapse_to_the_top_most_one() {
        let (_directory, mut lock) = temp_lock();
        assert!(
            lock.add_workspace_folder(Path::new("/work/project"))
                .expect("add outer")
        );
        // A nested folder must not be published: it would make the CLI match
        // this server twice.
        assert!(
            !lock
                .add_workspace_folder(Path::new("/work/project/crates/inner"))
                .expect("add nested")
        );
        assert_eq!(lock.workspace_folders(), vec![normalized("/work/project")]);
    }

    #[test]
    fn a_new_outer_root_replaces_previously_published_nested_ones() {
        let (_directory, mut lock) = temp_lock();
        lock.add_workspace_folder(Path::new("/work/project/crates/inner"))
            .expect("add inner");
        lock.add_workspace_folder(Path::new("/work/project"))
            .expect("add outer");

        assert_eq!(lock.workspace_folders(), vec![normalized("/work/project")]);
    }

    #[test]
    fn rewriting_with_unchanged_folders_reports_no_change() {
        let (_directory, mut lock) = temp_lock();
        lock.add_workspace_folder(Path::new("/work/project"))
            .expect("add folder");
        assert!(
            !lock
                .add_workspace_folder(Path::new("/work/project"))
                .expect("re-add folder")
        );
    }

    #[test]
    fn removing_a_folder_rewrites_the_file() {
        let (_directory, mut lock) = temp_lock();
        lock.add_workspace_folder(Path::new("/work/project"))
            .expect("add folder");
        assert!(
            lock.remove_workspace_folder(Path::new("/work/project"))
                .expect("remove folder")
        );

        let contents = std::fs::read_to_string(lock.path()).expect("read lock file");
        let payload: serde_json::Value = serde_json::from_str(&contents).expect("parse lock file");
        assert!(payload["workspaceFolders"].as_array().unwrap().is_empty());
    }

    #[test]
    fn replace_keeps_only_top_most_roots() {
        let (_directory, mut lock) = temp_lock();
        lock.replace_workspace_folders([
            PathBuf::from("/work/project/crates/inner"),
            PathBuf::from("/work/project"),
            PathBuf::from("/other"),
        ])
        .expect("replace folders");

        assert_eq!(
            lock.workspace_folders(),
            vec![normalized("/other"), normalized("/work/project")]
        );
    }

    #[test]
    fn remove_is_silent_when_nothing_was_written() {
        let (directory, lock) = temp_lock();
        lock.remove();
        assert!(!directory.path().join("1234.lock").exists());
    }
}
