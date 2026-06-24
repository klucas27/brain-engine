//! Integration tests for the scaffolding logic.
//!
//! These tests isolate `$HOME` to a temp dir so the global brain is created in
//! a sandbox and the developer's real `~/.brain` is never touched.

use std::path::Path;

use brain_core::config::{self, GlobalConfig, ProjectConfig};
use brain_core::db;
use brain_core::paths::{GlobalPaths, ProjectPaths};
use brain_core::scaffold;

/// Run `init_project` against a fresh temp dir and assert the full layout.
#[test]
fn init_project_creates_full_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let project = ProjectPaths::new(root.to_path_buf());

    let mut report = scaffold::InitReport::default();
    scaffold::init_project(&project, &mut report).unwrap();

    // Directories.
    for dir in project.directories() {
        assert!(dir.is_dir(), "expected dir to exist: {}", dir.display());
    }
    // Config + DB.
    assert!(project.config_file().is_file());
    assert!(project.metadata_db().is_file());
    assert!(report.db_created);
    assert!(project.is_initialised());

    // Schema is at head and the expected tables exist.
    let conn = db::open(&project.metadata_db()).unwrap();
    let (files, chunks) = db::counts(&conn).unwrap();
    assert_eq!(files, 0);
    assert_eq!(chunks, 0);
    assert_table_exists(&conn, "files");
    assert_table_exists(&conn, "chunks");
    assert_table_exists(&conn, "cache");
    assert_table_exists(&conn, "summaries");
    assert_table_exists(&conn, "symbols");
    assert_table_exists(&conn, "requests");
}

/// Running init twice must not error and must not duplicate gitignore entries.
#[test]
fn init_project_is_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let project = ProjectPaths::new(tmp.path().to_path_buf());

    let mut first = scaffold::InitReport::default();
    scaffold::init_project(&project, &mut first).unwrap();
    assert!(first.gitignore_updated);

    let mut second = scaffold::InitReport::default();
    scaffold::init_project(&project, &mut second).unwrap();
    // Nothing new should be created on the second pass.
    assert!(second.created_files.is_empty());
    assert!(second.created_dirs.is_empty());
    assert!(!second.db_created);
    assert!(!second.gitignore_updated);

    // `.brain/` appears exactly once in .gitignore.
    let gi = std::fs::read_to_string(project.gitignore_file()).unwrap();
    let occurrences = gi.lines().filter(|l| l.trim() == ".brain/").count();
    assert_eq!(occurrences, 1);
}

/// An existing `.gitignore` must be preserved, with the entry appended.
#[test]
fn init_preserves_existing_gitignore() {
    let tmp = tempfile::tempdir().unwrap();
    let project = ProjectPaths::new(tmp.path().to_path_buf());
    std::fs::write(project.gitignore_file(), "node_modules/\n").unwrap();

    let mut report = scaffold::InitReport::default();
    scaffold::init_project(&project, &mut report).unwrap();

    let gi = std::fs::read_to_string(project.gitignore_file()).unwrap();
    assert!(gi.contains("node_modules/"));
    assert!(gi.lines().any(|l| l.trim() == ".brain/"));
}

/// The global brain is created under an isolated home directory.
#[test]
fn init_global_seeds_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let global = GlobalPaths::with_home(tmp.path());

    let mut report = scaffold::InitReport::default();
    scaffold::init_global(&global, &mut report).unwrap();

    for dir in global.directories() {
        assert!(dir.is_dir(), "missing global dir: {}", dir.display());
    }
    // Defaults round-trip through serde.
    let cfg: GlobalConfig = config::load_or_default(&global.config_file()).unwrap();
    assert_eq!(cfg.version, config::GLOBAL_CONFIG_VERSION);
    assert_eq!(cfg.default_embedding, "local");
}

/// Project config default derives the project name from the directory.
#[test]
fn project_config_name_from_dir() {
    let cfg = ProjectConfig::for_root(Path::new("/tmp/my-cool-app"));
    assert_eq!(cfg.project_name, "my-cool-app");
}

fn assert_table_exists(conn: &rusqlite::Connection, name: &str) {
    let found: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(found, 1, "table {name} should exist");
}
