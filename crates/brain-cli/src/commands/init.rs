//! `brain init` — scaffold the global and project brains.

use std::path::Path;

use brain_core::scaffold::{self, InitReport};
use brain_core::Result;

/// Execute `brain init` for `root`. `json` selects machine-readable output.
pub fn run(root: &Path, json: bool) -> Result<()> {
    let report = scaffold::init(root)?;

    if json {
        print_json(root, &report);
    } else {
        print_human(root, &report);
    }
    Ok(())
}

fn print_human(root: &Path, report: &InitReport) {
    let created = report.created_dirs.len() + report.created_files.len();
    if created == 0 && !report.gitignore_updated {
        println!("✓ Brain already initialised for {}", root.display());
        return;
    }

    println!("✓ Brain initialised for {}", root.display());
    for dir in &report.created_dirs {
        println!("  + dir   {}", dir.display());
    }
    for file in &report.created_files {
        println!("  + file  {}", file.display());
    }
    if report.gitignore_updated {
        println!("  + ignore .brain/ added to .gitignore");
    }
    if report.db_created {
        println!("  • metadata database created and migrated to head");
    }
}

fn print_json(root: &Path, report: &InitReport) {
    let value = serde_json::json!({
        "root": root,
        "created_dirs": report.created_dirs,
        "created_files": report.created_files,
        "db_created": report.db_created,
        "gitignore_updated": report.gitignore_updated,
    });
    println!("{}", serde_json::to_string_pretty(&value).unwrap());
}
