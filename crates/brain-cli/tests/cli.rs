//! End-to-end tests driving the compiled `brain` binary.
//!
//! `HOME` is redirected to a temp dir so the global brain is sandboxed.
//!
//! Phase 4 tests cover `brain query` error paths that do not require the
//! ONNX model to be downloaded (they use `--no-embed` indexing).

use assert_cmd::Command;
use predicates::prelude::*;

/// Build a `brain` command with an isolated HOME and working directory.
fn brain(home: &std::path::Path, cwd: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("brain").unwrap();
    cmd.env("HOME", home).current_dir(cwd);
    cmd
}

#[test]
fn init_then_status_reports_ready() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();

    brain(home.path(), proj.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("Brain initialised"));

    brain(home.path(), proj.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Project brain [ready]"))
        .stdout(predicate::str::contains("schema      v4"));
}

#[test]
fn status_before_init_reports_missing() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();

    brain(home.path(), proj.path())
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("Project brain [missing]"));
}

#[test]
fn init_is_idempotent_via_cli() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();

    brain(home.path(), proj.path())
        .arg("init")
        .assert()
        .success();
    brain(home.path(), proj.path())
        .arg("init")
        .assert()
        .success()
        .stdout(predicate::str::contains("already initialised"));
}

#[test]
fn index_before_init_errors_with_guidance() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();

    brain(home.path(), proj.path())
        .arg("index")
        .assert()
        .failure()
        .stderr(predicate::str::contains("run `brain init` first"));
}

#[test]
fn init_index_status_end_to_end() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("main.rs"), "fn main() {}\n").unwrap();

    brain(home.path(), proj.path())
        .arg("init")
        .assert()
        .success();

    // First index reports a successful run that wrote chunks.
    // Use --no-embed to avoid downloading the ONNX model in CI.
    let out = brain(home.path(), proj.path())
        .args(["--json", "index", "--no-embed"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(first["indexed"].as_u64().unwrap() >= 1);
    assert!(first["chunks_written"].as_u64().unwrap() >= 1);

    // Re-indexing is a no-op.
    brain(home.path(), proj.path())
        .args(["--json", "index", "--no-embed"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"indexed\": 0"));

    // Status now reflects at least one indexed file.
    let out = brain(home.path(), proj.path())
        .args(["--json", "status"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let status: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(status["project"]["files_indexed"].as_u64().unwrap() >= 1);
}

#[test]
fn symbols_command_finds_indexed_symbol() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(
        proj.path().join("worker.rs"),
        "pub fn handle_query(input: &str) -> usize { input.len() }\n",
    )
    .unwrap();

    brain(home.path(), proj.path())
        .arg("init")
        .assert()
        .success();

    brain(home.path(), proj.path())
        .args(["index", "--no-embed"])
        .assert()
        .success();

    brain(home.path(), proj.path())
        .args(["symbols", "handle_query"])
        .assert()
        .success()
        .stdout(predicate::str::contains("worker.rs:1"))
        .stdout(predicate::str::contains("fn handle_query"));
}

#[test]
fn init_json_output_is_valid() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();

    let out = brain(home.path(), proj.path())
        .args(["--json", "init"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(parsed["db_created"], serde_json::Value::Bool(true));
}

// ---------------------------------------------------------------------------
// Phase 4 — brain query
// ---------------------------------------------------------------------------

#[test]
fn query_before_init_errors_with_guidance() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();

    brain(home.path(), proj.path())
        .args(["query", "what does main do"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("run `brain init` first"));
}

#[test]
fn query_before_index_reports_no_chunks() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("main.rs"), "fn main() {}\n").unwrap();

    brain(home.path(), proj.path())
        .arg("init")
        .assert()
        .success();

    // Query without ever running index.
    brain(home.path(), proj.path())
        .args(["query", "what does main do"])
        .assert()
        .success() // exits 0, prints guidance to stderr
        .stderr(predicate::str::contains("no chunks indexed"));
}

#[test]
fn query_without_embeddings_reports_missing_embeddings() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(proj.path().join("main.rs"), "fn main() {}\n").unwrap();

    brain(home.path(), proj.path())
        .arg("init")
        .assert()
        .success();

    // Index without embedding so the vector store stays empty.
    brain(home.path(), proj.path())
        .args(["index", "--no-embed"])
        .assert()
        .success();

    // Query should report that embeddings are missing, not panic.
    brain(home.path(), proj.path())
        .args(["query", "what does main do"])
        .assert()
        .success() // exits 0, prints guidance to stderr
        .stderr(predicate::str::contains("no embeddings found"));
}

#[test]
fn query_without_embeddings_json_is_valid_error_object() {
    let home = tempfile::tempdir().unwrap();
    let proj = tempfile::tempdir().unwrap();
    std::fs::write(
        proj.path().join("lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();

    brain(home.path(), proj.path())
        .arg("init")
        .assert()
        .success();

    brain(home.path(), proj.path())
        .args(["index", "--no-embed"])
        .assert()
        .success();

    let out = brain(home.path(), proj.path())
        .args(["--json", "query", "add two numbers"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    // Must be parseable JSON with an "error" key.
    let parsed: serde_json::Value =
        serde_json::from_slice(&out).expect("--json output must be valid JSON");
    assert!(
        parsed.get("error").is_some(),
        "expected 'error' key in: {parsed}"
    );
}
