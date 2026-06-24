//! Integration tests for Knowledge Digest summary generation.

use std::fs;
use std::path::Path;

use brain_core::config::ProjectConfig;
use brain_core::db;
use brain_core::index;
use brain_core::paths::ProjectPaths;
use brain_core::scaffold;
use brain_core::summarize;

fn setup_project() -> (tempfile::TempDir, ProjectPaths, ProjectConfig) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(
        root,
        "src/lib.rs",
        "//! Library entry point\n\npub struct Engine;\npub fn run() {}\n",
    );
    write(root, "README.md", "# Demo\n\nProject notes.\n");

    let project = ProjectPaths::new(root.to_path_buf());
    let mut report = scaffold::InitReport::default();
    scaffold::init_project(&project, &mut report).unwrap();

    let cfg = ProjectConfig::for_root(root);
    (tmp, project, cfg)
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

#[test]
fn sync_writes_file_module_and_project_summaries() {
    let (tmp, project, cfg) = setup_project();
    let mut conn = db::open(&project.metadata_db()).unwrap();
    index::index_project(tmp.path(), &cfg, &mut conn).unwrap();

    let stats =
        summarize::sync_project_summaries(tmp.path(), &project.summaries_dir(), &cfg, &conn)
            .unwrap();

    assert!(stats.files_seen >= 2);
    assert_eq!(stats.file_summaries_written, stats.files_seen);
    assert!(stats.module_summaries_written >= 1);
    assert!(stats.project_summary_written);
    assert!(project.summaries_dir().join("src/lib.rs.md").is_file());
    assert!(project.summaries_dir().join("PROJECT.md").is_file());

    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM summaries", [], |r| r.get(0))
        .unwrap();
    assert!(count >= 4, "expected file/module/project summaries");
}

#[test]
fn sync_is_incremental_for_unchanged_files() {
    let (tmp, project, cfg) = setup_project();
    let mut conn = db::open(&project.metadata_db()).unwrap();
    index::index_project(tmp.path(), &cfg, &mut conn).unwrap();

    summarize::sync_project_summaries(tmp.path(), &project.summaries_dir(), &cfg, &conn)
        .unwrap();
    let second =
        summarize::sync_project_summaries(tmp.path(), &project.summaries_dir(), &cfg, &conn)
            .unwrap();

    assert_eq!(second.file_summaries_written, 0);
    assert_eq!(second.module_summaries_written, 0);
    assert!(!second.project_summary_written);
}

#[test]
fn editing_one_file_regenerates_only_that_file_summary() {
    let (tmp, project, cfg) = setup_project();
    let mut conn = db::open(&project.metadata_db()).unwrap();
    index::index_project(tmp.path(), &cfg, &mut conn).unwrap();
    summarize::sync_project_summaries(tmp.path(), &project.summaries_dir(), &cfg, &conn)
        .unwrap();

    write(
        tmp.path(),
        "src/lib.rs",
        "//! Changed library\n\npub struct Engine;\npub fn run() {}\npub fn stop() {}\n",
    );
    index::index_project(tmp.path(), &cfg, &mut conn).unwrap();
    let stats =
        summarize::sync_project_summaries(tmp.path(), &project.summaries_dir(), &cfg, &conn)
            .unwrap();

    assert_eq!(stats.file_summaries_written, 1);
    assert!(stats.module_summaries_written >= 1);
    assert!(stats.project_summary_written);
}
