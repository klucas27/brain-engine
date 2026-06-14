//! Integration tests for the Phase 2 indexer.
//!
//! Each test builds a real project tree in a temp dir, initialises a brain, and
//! drives `index_project` against the live SQLite database.

use std::fs;
use std::path::Path;

use brain_core::config::ProjectConfig;
use brain_core::db;
use brain_core::index::{self, IndexStats};
use brain_core::paths::ProjectPaths;
use brain_core::scaffold;

/// Create an initialised project containing a small, representative tree.
fn setup_project() -> (tempfile::TempDir, ProjectConfig) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();

    // Source files (indexable).
    write(
        root,
        "src/main.rs",
        "fn main() {\n    println!(\"hi\");\n}\n",
    );
    write(
        root,
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    );
    write(root, "README.md", "# Title\n\nSome docs.\n");

    // A secret (excluded by default glob).
    write(root, ".env", "API_KEY=supersecret\n");
    // A vendored dependency (excluded by default glob).
    write(root, "node_modules/dep/index.js", "module.exports = 1;\n");
    // A binary file (skipped during read).
    fs::create_dir_all(root.join("assets")).unwrap();
    fs::write(root.join("assets/logo.bin"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();

    let project = ProjectPaths::new(root.to_path_buf());
    let mut report = scaffold::InitReport::default();
    scaffold::init_project(&project, &mut report).unwrap();

    let cfg = ProjectConfig::for_root(root);
    (tmp, cfg)
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}

fn run_index(root: &Path, cfg: &ProjectConfig) -> IndexStats {
    let mut conn = db::open(&ProjectPaths::new(root.to_path_buf()).metadata_db()).unwrap();
    index::index_project(root, cfg, &mut conn).unwrap()
}

#[test]
fn first_run_indexes_source_and_skips_secrets_and_binaries() {
    let (tmp, cfg) = setup_project();
    let root = tmp.path();

    let stats = run_index(root, &cfg);

    // The binary is scanned but skipped; secrets/vendored/own-config are excluded
    // by globs before the scan even sees them.
    assert_eq!(stats.skipped_binary, 1, "stats: {stats:?}");
    assert!(stats.skipped_excluded >= 3); // .env + node_modules + brain.config.json
    assert!(stats.chunks_written >= 3);

    // The DB reflects exactly the files the run reported as indexed.
    let conn = db::open(&ProjectPaths::new(root.to_path_buf()).metadata_db()).unwrap();
    let (files, chunks) = db::counts(&conn).unwrap();
    assert_eq!(files as usize, stats.indexed);
    assert!(chunks >= 3);

    // Source is indexed; secrets, binaries and the brain's own config are not.
    assert!(path_indexed(&conn, "src/main.rs"));
    assert!(path_indexed(&conn, "src/lib.rs"));
    assert!(path_indexed(&conn, "README.md"));
    assert!(!path_indexed(&conn, ".env"));
    assert!(!path_indexed(&conn, "assets/logo.bin"));
    assert!(!path_indexed(&conn, "brain.config.json"));
    assert!(!path_indexed(&conn, "node_modules/dep/index.js"));
}

#[test]
fn second_run_is_a_noop() {
    let (tmp, cfg) = setup_project();
    let root = tmp.path();

    let first = run_index(root, &cfg);
    let second = run_index(root, &cfg);

    assert_eq!(second.indexed, 0);
    assert_eq!(second.chunks_written, 0);
    assert_eq!(second.removed, 0);
    assert_eq!(second.unchanged, first.indexed);
}

#[test]
fn editing_one_file_reindexes_only_that_file() {
    let (tmp, cfg) = setup_project();
    let root = tmp.path();
    run_index(root, &cfg);

    // Sleep is unnecessary: the indexer falls back to a hash compare when mtime
    // changes, and rewriting content guarantees a new hash anyway.
    write(
        root,
        "src/lib.rs",
        "pub fn add(a: i32, b: i32) -> i32 { a + b + 1 }\n// changed\n",
    );

    let stats = run_index(root, &cfg);
    assert_eq!(stats.indexed, 1, "stats: {stats:?}");
    assert_eq!(stats.removed, 0);
}

#[test]
fn deleting_a_file_removes_it_from_the_index() {
    let (tmp, cfg) = setup_project();
    let root = tmp.path();
    let before = run_index(root, &cfg).indexed as i64;

    fs::remove_file(root.join("README.md")).unwrap();

    let stats = run_index(root, &cfg);
    assert_eq!(stats.removed, 1);

    let conn = db::open(&ProjectPaths::new(root.to_path_buf()).metadata_db()).unwrap();
    assert!(!path_indexed(&conn, "README.md"));
    let (files, _) = db::counts(&conn).unwrap();
    assert_eq!(files, before - 1);
}

#[test]
fn touching_without_changing_content_does_not_reindex() {
    let (tmp, cfg) = setup_project();
    let root = tmp.path();
    run_index(root, &cfg);

    // Rewrite identical content to bump mtime but keep the hash.
    write(root, "README.md", "# Title\n\nSome docs.\n");

    let stats = run_index(root, &cfg);
    assert_eq!(stats.indexed, 0, "content unchanged: {stats:?}");
    assert_eq!(stats.removed, 0);
}

#[test]
fn large_files_are_skipped() {
    let (tmp, mut cfg) = setup_project();
    let root = tmp.path();
    cfg.max_file_size_bytes = 30; // tiny cap

    write(root, "src/huge.rs", &"x".repeat(500));

    let stats = run_index(root, &cfg);
    // huge.rs exceeds the cap; the small sources stay indexed.
    assert!(stats.skipped_large >= 1, "stats: {stats:?}");
    let conn = db::open(&ProjectPaths::new(root.to_path_buf()).metadata_db()).unwrap();
    assert!(!path_indexed(&conn, "src/huge.rs"));
}

fn path_indexed(conn: &rusqlite::Connection, rel: &str) -> bool {
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM files WHERE path = ?1", [rel], |r| {
            r.get(0)
        })
        .unwrap();
    n == 1
}
