//! Knowledge Digest generation.
//!
//! The default summarizer is deterministic and local: it extracts top-level
//! comments, visible declarations and short structural hints. This keeps
//! indexing cheap while creating a persistent, low-token knowledge layer that
//! later retrieval phases can consult before falling back to raw chunks.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::config::ProjectConfig;
use crate::error::{BrainError, Result};
use crate::hash::sha256_hex;
use crate::tokens;

const MODEL_HEURISTIC: &str = "heuristic";
const PROJECT_TARGET: &str = "PROJECT";

/// Summary of a digest synchronization run.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SummaryStats {
    pub files_seen: usize,
    pub file_summaries_written: usize,
    pub module_summaries_written: usize,
    pub project_summary_written: bool,
    pub removed: usize,
}

#[derive(Debug, Clone)]
struct FileRow {
    path: String,
    hash: String,
    lang: Option<String>,
}

#[derive(Debug, Clone)]
struct ExistingSummary {
    source_hash: String,
}

#[derive(Debug, Clone)]
struct FileSummary {
    path: String,
    hash: String,
    summary: String,
}

/// Synchronize summaries with the current `files` table.
///
/// Only file summaries whose `source_hash` differs from the indexed file hash
/// are regenerated. Module and project summaries are derived from file
/// summaries and updated only when their aggregate hash changes.
pub fn sync_project_summaries(
    root: &Path,
    summaries_dir: &Path,
    cfg: &ProjectConfig,
    conn: &Connection,
) -> Result<SummaryStats> {
    let mut stats = SummaryStats::default();
    if !cfg.summaries.enabled {
        return Ok(stats);
    }

    std::fs::create_dir_all(summaries_dir).map_err(|e| BrainError::io(summaries_dir, e))?;

    let files = load_files(conn)?;
    stats.files_seen = files.len();
    let existing = load_existing_summaries(conn)?;
    let now = now_secs();

    let valid_file_targets: BTreeSet<String> = files.iter().map(|f| f.path.clone()).collect();
    stats.removed += delete_stale_file_summaries(conn, &valid_file_targets)?;

    let mut file_summaries = Vec::with_capacity(files.len());
    for file in files {
        let existing_key = ("file".to_string(), file.path.clone());
        if let Some(prev) = existing.get(&existing_key) {
            if prev.source_hash == file.hash {
                if let Some(summary) = read_summary(conn, "file", &file.path)? {
                    file_summaries.push(FileSummary {
                        path: file.path,
                        hash: file.hash,
                        summary,
                    });
                    continue;
                }
            }
        }

        let abs = root.join(&file.path);
        let content = match std::fs::read_to_string(&abs) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(BrainError::io(abs, e)),
        };
        let summary = summarize_file(&file.path, &content, file.lang.as_deref(), cfg);
        upsert_summary(
            conn,
            "file",
            &file.path,
            &summary,
            &file.hash,
            MODEL_HEURISTIC,
            now,
        )?;
        write_summary_mirror(summaries_dir, "file", &file.path, &summary)?;
        stats.file_summaries_written += 1;
        file_summaries.push(FileSummary {
            path: file.path,
            hash: file.hash,
            summary,
        });
    }

    let module_summaries = sync_module_summaries(summaries_dir, conn, &file_summaries, now)?;
    stats.module_summaries_written = module_summaries;

    stats.project_summary_written =
        sync_project_summary(summaries_dir, conn, &file_summaries, now)?;

    Ok(stats)
}

/// Build a short, deterministic file summary.
pub fn summarize_file(path: &str, content: &str, lang: Option<&str>, cfg: &ProjectConfig) -> String {
    let mut lines = Vec::new();
    lines.push(format!(
        "File `{}`{}.",
        path,
        lang.map(|l| format!(" ({l})")).unwrap_or_default()
    ));

    let doc = leading_comments(content);
    if !doc.is_empty() {
        lines.push(format!("Top notes: {}.", doc.join(" ")));
    }

    let declarations = declarations(content);
    if !declarations.is_empty() {
        lines.push(format!("Key symbols: {}.", declarations.join("; ")));
    }

    if lines.len() == 1 {
        let first = content
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("No visible text content.");
        lines.push(format!("Starts with: {}.", truncate_chars(first, 160)));
    }

    clamp_tokens(&lines.join(" "), cfg.summaries.max_summary_tokens)
}

fn load_files(conn: &Connection) -> Result<Vec<FileRow>> {
    let mut stmt = conn.prepare("SELECT path, hash, lang FROM files ORDER BY path")?;
    let rows = stmt.query_map([], |r| {
        Ok(FileRow {
            path: r.get(0)?,
            hash: r.get(1)?,
            lang: r.get(2)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn load_existing_summaries(conn: &Connection) -> Result<HashMap<(String, String), ExistingSummary>> {
    let mut stmt = conn.prepare("SELECT scope, target, source_hash FROM summaries")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            ExistingSummary {
                source_hash: r.get(2)?,
            },
        ))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (key, value) = row?;
        out.insert(key, value);
    }
    Ok(out)
}

fn read_summary(conn: &Connection, scope: &str, target: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT summary FROM summaries WHERE scope = ?1 AND target = ?2",
            params![scope, target],
            |r| r.get(0),
        )
        .optional()?)
}

fn delete_stale_file_summaries(conn: &Connection, valid_targets: &BTreeSet<String>) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT target FROM summaries WHERE scope = 'file'")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut removed = 0usize;
    for row in rows {
        let target = row?;
        if !valid_targets.contains(&target) {
            conn.execute(
                "DELETE FROM summaries WHERE scope = 'file' AND target = ?1",
                [target],
            )?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn sync_module_summaries(
    summaries_dir: &Path,
    conn: &Connection,
    files: &[FileSummary],
    now: i64,
) -> Result<usize> {
    let mut by_module: BTreeMap<String, Vec<&FileSummary>> = BTreeMap::new();
    for file in files {
        by_module.entry(module_target(&file.path)).or_default().push(file);
    }

    let valid_modules: BTreeSet<String> = by_module.keys().cloned().collect();
    delete_stale_scoped_summaries(conn, "module", &valid_modules)?;

    let mut written = 0usize;
    for (module, mut module_files) in by_module {
        module_files.sort_by(|a, b| a.path.cmp(&b.path));
        let source_hash = aggregate_hash(module_files.iter().map(|f| f.hash.as_str()));
        let current_hash = source_hash_for(conn, "module", &module)?;
        if current_hash.as_deref() == Some(source_hash.as_str()) {
            continue;
        }
        let summary = summarize_module(&module, &module_files);
        upsert_summary(conn, "module", &module, &summary, &source_hash, MODEL_HEURISTIC, now)?;
        write_summary_mirror(summaries_dir, "module", &module, &summary)?;
        written += 1;
    }
    Ok(written)
}

fn sync_project_summary(
    summaries_dir: &Path,
    conn: &Connection,
    files: &[FileSummary],
    now: i64,
) -> Result<bool> {
    let source_hash = aggregate_hash(files.iter().map(|f| f.hash.as_str()));
    let current_hash = source_hash_for(conn, "project", PROJECT_TARGET)?;
    if current_hash.as_deref() == Some(source_hash.as_str()) {
        return Ok(false);
    }
    let summary = summarize_project(files);
    upsert_summary(
        conn,
        "project",
        PROJECT_TARGET,
        &summary,
        &source_hash,
        MODEL_HEURISTIC,
        now,
    )?;
    write_summary_mirror(summaries_dir, "project", PROJECT_TARGET, &summary)?;
    Ok(true)
}

fn summarize_module(module: &str, files: &[&FileSummary]) -> String {
    let mut parts = vec![format!("Module `{module}` contains {} indexed file(s).", files.len())];
    for file in files.iter().take(8) {
        parts.push(format!(
            "- `{}`: {}",
            file.path,
            truncate_chars(&file.summary, 180)
        ));
    }
    parts.join("\n")
}

fn summarize_project(files: &[FileSummary]) -> String {
    let mut modules: BTreeMap<String, usize> = BTreeMap::new();
    for file in files {
        *modules.entry(module_target(&file.path)).or_default() += 1;
    }
    let module_list = modules
        .iter()
        .take(12)
        .map(|(module, count)| format!("{module} ({count})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Project has {} indexed file(s) across {} module(s). Main modules: {}.",
        files.len(),
        modules.len(),
        if module_list.is_empty() { "none".to_string() } else { module_list }
    )
}

fn source_hash_for(conn: &Connection, scope: &str, target: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT source_hash FROM summaries WHERE scope = ?1 AND target = ?2",
            params![scope, target],
            |r| r.get(0),
        )
        .optional()?)
}

fn upsert_summary(
    conn: &Connection,
    scope: &str,
    target: &str,
    summary: &str,
    source_hash: &str,
    model_used: &str,
    created_at: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO summaries(scope, target, summary, source_hash, token_estimate, model_used, created_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(scope, target) DO UPDATE SET
            summary = excluded.summary,
            source_hash = excluded.source_hash,
            token_estimate = excluded.token_estimate,
            model_used = excluded.model_used,
            created_at = excluded.created_at",
        params![
            scope,
            target,
            summary,
            source_hash,
            tokens::estimate(summary) as i64,
            model_used,
            created_at
        ],
    )?;
    Ok(())
}

fn delete_stale_scoped_summaries(
    conn: &Connection,
    scope: &str,
    valid_targets: &BTreeSet<String>,
) -> Result<usize> {
    let mut stmt = conn.prepare("SELECT target FROM summaries WHERE scope = ?1")?;
    let rows = stmt.query_map([scope], |r| r.get::<_, String>(0))?;
    let mut removed = 0usize;
    for row in rows {
        let target = row?;
        if !valid_targets.contains(&target) {
            conn.execute(
                "DELETE FROM summaries WHERE scope = ?1 AND target = ?2",
                params![scope, target],
            )?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn write_summary_mirror(
    summaries_dir: &Path,
    scope: &str,
    target: &str,
    summary: &str,
) -> Result<()> {
    let path = mirror_path(summaries_dir, scope, target);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| BrainError::io(parent, e))?;
    }
    std::fs::write(&path, format!("{summary}\n")).map_err(|e| BrainError::io(path, e))?;
    Ok(())
}

fn mirror_path(summaries_dir: &Path, scope: &str, target: &str) -> PathBuf {
    match scope {
        "file" => summaries_dir.join(format!("{target}.md")),
        "module" => summaries_dir.join(target).join("_module.md"),
        "project" => summaries_dir.join("PROJECT.md"),
        _ => summaries_dir.join(format!("{}_{}.md", scope, target.replace('/', "_"))),
    }
}

fn module_target(path: &str) -> String {
    Path::new(path)
        .parent()
        .and_then(|p| p.to_str())
        .filter(|p| !p.is_empty())
        .unwrap_or(".")
        .to_string()
}

fn aggregate_hash<'a>(hashes: impl Iterator<Item = &'a str>) -> String {
    let joined = hashes.collect::<Vec<_>>().join("\n");
    sha256_hex(joined.as_bytes())
}

fn leading_comments(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines().take(30) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if out.is_empty() {
                continue;
            }
            break;
        }
        let comment = trimmed
            .strip_prefix("//!")
            .or_else(|| trimmed.strip_prefix("///"))
            .or_else(|| trimmed.strip_prefix("//"))
            .or_else(|| trimmed.strip_prefix("# "))
            .or_else(|| trimmed.strip_prefix("* "));
        match comment {
            Some(text) => {
                let cleaned = text.trim().trim_matches('*').trim();
                if !cleaned.is_empty() {
                    out.push(truncate_chars(cleaned, 180));
                }
            }
            None if out.is_empty() && looks_like_shebang(trimmed) => continue,
            None => break,
        }
        if out.len() >= 4 {
            break;
        }
    }
    out
}

fn looks_like_shebang(line: &str) -> bool {
    line.starts_with("#!")
}

fn declarations(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }
        if is_declaration(trimmed) {
            out.push(truncate_chars(trimmed, 140));
        }
        if out.len() >= 12 {
            break;
        }
    }
    out
}

fn is_declaration(line: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "pub fn ",
        "fn ",
        "pub struct ",
        "struct ",
        "pub enum ",
        "enum ",
        "pub trait ",
        "trait ",
        "impl ",
        "export function ",
        "function ",
        "export const ",
        "const ",
        "let ",
        "class ",
        "export class ",
        "def ",
        "async def ",
        "type ",
        "interface ",
    ];
    PREFIXES.iter().any(|prefix| line.starts_with(prefix))
}

fn clamp_tokens(text: &str, max_tokens: usize) -> String {
    if max_tokens == 0 || tokens::estimate(text) <= max_tokens {
        return text.to_string();
    }
    truncate_chars(text, max_tokens.saturating_mul(4))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out = text.chars().take(max_chars.saturating_sub(1)).collect::<String>();
    out.push_str("...");
    out
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_file_extracts_comments_and_declarations() {
        let cfg = ProjectConfig::default();
        let summary = summarize_file(
            "src/lib.rs",
            "//! Core library\n\npub struct Engine;\npub fn run() {}\n",
            Some("rust"),
            &cfg,
        );

        assert!(summary.contains("Core library"));
        assert!(summary.contains("pub struct Engine"));
        assert!(summary.contains("pub fn run()"));
    }

    #[test]
    fn module_target_uses_parent_directory() {
        assert_eq!(module_target("src/lib.rs"), "src");
        assert_eq!(module_target("README.md"), ".");
    }
}
