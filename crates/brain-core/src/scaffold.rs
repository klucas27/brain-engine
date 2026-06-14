//! Idempotent scaffolding for the global and project brains.
//!
//! [`init`] is the single entry point behind the `brain init` command and the
//! future automatic bootstrap (Phase 8). It is deliberately non-destructive:
//! existing config and data are never overwritten, so it is safe to run on
//! every Claude Code launch.

use std::path::{Path, PathBuf};

use crate::config::{self, GlobalConfig, ProjectConfig, Providers};
use crate::db;
use crate::error::{BrainError, Result};
use crate::paths::{GlobalPaths, ProjectPaths};

/// A record of what `init` actually changed, so the CLI can report precisely
/// (created vs. already-present) instead of guessing.
#[derive(Debug, Default)]
pub struct InitReport {
    /// Directories created during this run.
    pub created_dirs: Vec<PathBuf>,
    /// Files created during this run.
    pub created_files: Vec<PathBuf>,
    /// Whether the metadata database was freshly created.
    pub db_created: bool,
    /// Whether `.brain/` was added to the project `.gitignore`.
    pub gitignore_updated: bool,
}

/// Initialise (or repair) both the global and the project brain for `root`.
///
/// Steps, all idempotent:
/// 1. ensure `~/.brain/` directories + default `config.json` / `providers.json`
/// 2. ensure `<root>/.brain/` directories
/// 3. ensure `<root>/brain.config.json`
/// 4. create/migrate `<root>/.brain/metadata.db`
/// 5. ensure `.brain/` is git-ignored
pub fn init(root: &Path) -> Result<InitReport> {
    let mut report = InitReport::default();

    let global = GlobalPaths::resolve()?;
    init_global(&global, &mut report)?;

    let project = ProjectPaths::new(root.to_path_buf());
    init_project(&project, &mut report)?;

    Ok(report)
}

/// Create the global brain layout and seed default config files.
pub fn init_global(global: &GlobalPaths, report: &mut InitReport) -> Result<()> {
    for dir in global.directories() {
        create_dir(&dir, report)?;
    }
    if config::save_if_absent(&global.config_file(), &GlobalConfig::default())? {
        report.created_files.push(global.config_file());
    }
    if config::save_if_absent(&global.providers_file(), &Providers::default())? {
        report.created_files.push(global.providers_file());
    }
    Ok(())
}

/// Create the project brain layout, config, database and gitignore entry.
pub fn init_project(project: &ProjectPaths, report: &mut InitReport) -> Result<()> {
    if !project.root.is_dir() {
        return Err(BrainError::NotADirectory(project.root.clone()));
    }

    for dir in project.directories() {
        create_dir(&dir, report)?;
    }

    let cfg = ProjectConfig::for_root(&project.root);
    if config::save_if_absent(&project.config_file(), &cfg)? {
        report.created_files.push(project.config_file());
    }

    let db_path = project.metadata_db();
    let db_existed = db_path.exists();
    // Opening runs migrations and creates the file if missing.
    let _conn = db::open(&db_path)?;
    if !db_existed {
        report.db_created = true;
        report.created_files.push(db_path);
    }

    report.gitignore_updated = ensure_gitignore(&project.gitignore_file())?;

    Ok(())
}

/// Create a directory (and parents), recording it if newly created.
fn create_dir(dir: &Path, report: &mut InitReport) -> Result<()> {
    if dir.exists() {
        if !dir.is_dir() {
            return Err(BrainError::NotADirectory(dir.to_path_buf()));
        }
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| BrainError::io(dir, e))?;
    report.created_dirs.push(dir.to_path_buf());
    Ok(())
}

/// Marker line written to `.gitignore` so the project brain is never committed.
const GITIGNORE_ENTRY: &str = ".brain/";

/// Ensure `.brain/` is ignored by git. Returns `true` if the file was modified.
///
/// The brain holds embeddings, caches and a potentially large SQLite DB that
/// must not pollute the repository, so this is part of a healthy init.
fn ensure_gitignore(gitignore: &Path) -> Result<bool> {
    let existing = match std::fs::read_to_string(gitignore) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(BrainError::io(gitignore, e)),
    };

    let already = existing
        .lines()
        .map(str::trim)
        .any(|l| l == GITIGNORE_ENTRY || l == ".brain" || l == "/.brain/" || l == "/.brain");
    if already {
        return Ok(false);
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("\n# Brain Engine local data (do not commit)\n");
    updated.push_str(GITIGNORE_ENTRY);
    updated.push('\n');

    std::fs::write(gitignore, updated).map_err(|e| BrainError::io(gitignore, e))?;
    Ok(true)
}
