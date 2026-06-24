//! `brain symbols` — lookup indexed code symbols.

use std::path::Path;

use brain_core::db;
use brain_core::paths::ProjectPaths;
use brain_core::symbols;
use brain_core::{BrainError, Result};

pub fn run(
    root: &Path,
    json: bool,
    name: Option<&str>,
    kind: Option<&str>,
    limit: usize,
) -> Result<()> {
    let project = ProjectPaths::new(root.to_path_buf());
    if !project.is_initialised() {
        return Err(BrainError::Walk(format!(
            "no brain found at {} — run `brain init` first",
            project.brain_dir().display()
        )));
    }

    let conn = db::open(&project.metadata_db())?;
    let rows = symbols::search(&conn, name, kind, limit)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "symbols": rows
        }))?);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No symbols found.");
        return Ok(());
    }

    for row in rows {
        let lines = if row.start_line == row.end_line {
            row.start_line.to_string()
        } else {
            format!("{}-{}", row.start_line, row.end_line)
        };
        println!("{}:{}  {} {}", row.file, lines, row.kind, row.name);
        if let Some(sig) = row.signature {
            println!("  {sig}");
        }
        if let Some(doc) = row.doc {
            println!("  doc: {doc}");
        }
    }
    Ok(())
}
