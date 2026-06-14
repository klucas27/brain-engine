//! `brain status` — report the health of the global and project brains.

use std::path::Path;

use brain_core::config::{self, GlobalConfig, ProjectConfig};
use brain_core::db;
use brain_core::paths::{GlobalPaths, ProjectPaths};
use brain_core::vectors::VectorStore;
use brain_core::Result;

/// Execute `brain status` for `root`.
pub fn run(root: &Path, json: bool) -> Result<()> {
    let global = GlobalPaths::resolve()?;
    let project = ProjectPaths::new(root.to_path_buf());

    let global_ready = global.config_file().exists();
    let project_ready = project.is_initialised();

    let gcfg: GlobalConfig = config::load_or_default(&global.config_file())?;
    let pcfg: ProjectConfig = config::load_or_default(&project.config_file())?;

    // Database + vector-store facts — only populated when the brain is ready.
    let (schema_version, files, chunks, embedding_model, embedding_dim, vectors) = if project_ready
    {
        let conn = db::open(&project.metadata_db())?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let (f, c) = db::counts(&conn)?;
        let model = db::get_meta(&conn, "embedding.model_id")?;
        let dim: Option<usize> = db::get_meta(&conn, "embedding.dim")?.and_then(|s| s.parse().ok());
        // Open vector store; return 0 if it doesn't exist yet.
        let vs = VectorStore::open(&project.vectors_dir())?;
        let v = vs.count()?;
        (version, f, c, model, dim, v as i64)
    } else {
        (0, 0, 0, None, None, 0)
    };

    if json {
        let value = serde_json::json!({
            "version": brain_core::VERSION,
            "global": {
                "root": global.root,
                "ready": global_ready,
                "default_embedding": gcfg.default_embedding,
                "default_llm": gcfg.default_llm
            },
            "project": {
                "root": project.root,
                "ready": project_ready,
                "name": pcfg.project_name,
                "embedding_provider": pcfg.embedding_provider,
                "schema_version": schema_version,
                "files_indexed": files,
                "chunks_indexed": chunks,
                "embedding_model": embedding_model,
                "embedding_dim": embedding_dim,
                "vectors_stored": vectors
            },
        });
        println!("{}", serde_json::to_string_pretty(&value).unwrap());
        return Ok(());
    }

    println!("Brain Engine v{}", brain_core::VERSION);
    println!();
    println!("Global brain  {}", yn(global_ready));
    println!("  path        {}", global.root.display());
    println!("  embedding   {}", gcfg.default_embedding);
    println!("  llm         {}", gcfg.default_llm);
    println!();
    println!("Project brain {}", yn(project_ready));
    println!("  path        {}", project.brain_dir().display());
    if project_ready {
        println!("  name        {}", pcfg.project_name);
        println!("  embedding   {}", pcfg.embedding_provider);
        println!("  schema      v{schema_version}");
        println!("  files       {files}");
        println!("  chunks      {chunks}");
        println!("  vectors     {vectors}");
        let model_str = match (embedding_model.as_deref(), embedding_dim) {
            (Some(m), Some(d)) => format!("{m} (dim {d})"),
            (Some(m), None) => m.to_string(),
            _ => "(not pinned yet — run `brain index`)".to_string(),
        };
        println!("  model       {model_str}");
    } else {
        println!("  (run `brain init` to set up this project)");
    }

    Ok(())
}

fn yn(ready: bool) -> &'static str {
    if ready {
        "[ready]"
    } else {
        "[missing]"
    }
}
