//! Integration tests for persisted symbol lookup.

use std::fs;
use std::path::Path;

use brain_core::config::ProjectConfig;
use brain_core::db;
use brain_core::index;
use brain_core::paths::ProjectPaths;
use brain_core::scaffold;
use brain_core::symbols;

#[test]
fn search_finds_indexed_symbol_by_name() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    write(root, "src/lib.rs", "pub fn handle_query() {}\npub struct Worker;\n");

    let project = ProjectPaths::new(root.to_path_buf());
    let mut report = scaffold::InitReport::default();
    scaffold::init_project(&project, &mut report).unwrap();
    let cfg = ProjectConfig::for_root(root);
    let mut conn = db::open(&project.metadata_db()).unwrap();
    index::index_project(root, &cfg, &mut conn).unwrap();

    let rows = symbols::search(&conn, Some("handle_query"), None, 20).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].file, "src/lib.rs");
    assert_eq!(rows[0].kind, "fn");
    assert!(rows[0].signature.as_deref().unwrap().contains("handle_query"));
}

fn write(root: &Path, rel: &str, contents: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, contents).unwrap();
}
