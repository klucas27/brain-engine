//! Lightweight code symbol extraction and lookup.
//!
//! This intentionally starts with deterministic line-based extractors for the
//! languages used most in this project and common frontend repos. The public
//! API is small enough to swap the implementation for tree-sitter later.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// A symbol extracted from one source file before persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedSymbol {
    pub name: String,
    pub kind: String,
    pub signature: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub visibility: Option<String>,
    pub doc: Option<String>,
}

/// A persisted symbol enriched with its file path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolRecord {
    pub file: String,
    pub name: String,
    pub kind: String,
    pub signature: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub visibility: Option<String>,
    pub doc: Option<String>,
}

/// Extract symbols from source text.
pub fn extract(content: &str, lang: Option<&str>) -> Vec<ExtractedSymbol> {
    match lang.unwrap_or_default() {
        "rust" => extract_rust(content),
        "typescript" | "javascript" => extract_ts_js(content),
        "python" => extract_python(content),
        _ => Vec::new(),
    }
}

/// Replace all symbols for `file_id`.
pub fn replace_for_file(
    conn: &Connection,
    file_id: i64,
    symbols: &[ExtractedSymbol],
) -> Result<()> {
    conn.execute("DELETE FROM symbols WHERE file_id = ?1", [file_id])?;
    let mut stmt = conn.prepare(
        "INSERT INTO symbols(file_id, name, kind, signature, start_line, end_line, visibility, doc)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for symbol in symbols {
        stmt.execute(params![
            file_id,
            symbol.name,
            symbol.kind,
            symbol.signature,
            symbol.start_line as i64,
            symbol.end_line as i64,
            symbol.visibility,
            symbol.doc
        ])?;
    }
    Ok(())
}

/// Query symbols by optional exact-ish name and kind.
///
/// Name matching is case-sensitive exact first, with caller-controlled partial
/// lookup via SQL `LIKE` so CLI searches remain forgiving.
pub fn search(
    conn: &Connection,
    name: Option<&str>,
    kind: Option<&str>,
    limit: usize,
) -> Result<Vec<SymbolRecord>> {
    let limit = limit.max(1).min(200) as i64;
    let pattern = name.map(|n| format!("%{n}%"));

    let mut sql = String::from(
        "SELECT f.path, s.name, s.kind, s.signature, s.start_line, s.end_line, s.visibility, s.doc
         FROM symbols s JOIN files f ON f.id = s.file_id",
    );
    let mut clauses = Vec::new();
    if name.is_some() {
        clauses.push("(s.name = ?1 OR s.name LIKE ?2)");
    }
    if kind.is_some() {
        clauses.push("s.kind = ?3");
    }
    if !clauses.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&clauses.join(" AND "));
    }
    sql.push_str(" ORDER BY CASE WHEN s.name = ?1 THEN 0 ELSE 1 END, f.path, s.start_line LIMIT ?4");

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![name.unwrap_or_default(), pattern.as_deref().unwrap_or("%"), kind, limit],
        |r| {
            Ok(SymbolRecord {
                file: r.get(0)?,
                name: r.get(1)?,
                kind: r.get(2)?,
                signature: r.get(3)?,
                start_line: r.get::<_, i64>(4)? as usize,
                end_line: r.get::<_, i64>(5)? as usize,
                visibility: r.get(6)?,
                doc: r.get(7)?,
            })
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn extract_rust(content: &str) -> Vec<ExtractedSymbol> {
    let mut out = Vec::new();
    let mut docs = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if let Some(doc) = rust_doc(trimmed) {
            docs.push(doc.to_string());
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            if !trimmed.starts_with("#[") {
                docs.clear();
            }
            continue;
        }
        if let Some(symbol) = parse_rust_decl(trimmed, line_no, take_docs(&mut docs)) {
            out.push(symbol);
        } else {
            docs.clear();
        }
    }
    out
}

fn rust_doc(line: &str) -> Option<&str> {
    line.strip_prefix("///")
        .or_else(|| line.strip_prefix("//!"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn parse_rust_decl(line: &str, line_no: usize, doc: Option<String>) -> Option<ExtractedSymbol> {
    let visibility = line.starts_with("pub ").then(|| "pub".to_string());
    let s = line.strip_prefix("pub ").unwrap_or(line);
    let specs = [
        ("async fn ", "fn"),
        ("fn ", "fn"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("trait ", "trait"),
        ("type ", "type"),
        ("const ", "const"),
        ("impl ", "impl"),
    ];
    for (prefix, kind) in specs {
        if let Some(rest) = s.strip_prefix(prefix) {
            let name = if kind == "impl" {
                rest.split_whitespace()
                    .next()
                    .unwrap_or("impl")
                    .trim_matches('{')
                    .to_string()
            } else {
                symbol_name(rest)?
            };
            return Some(ExtractedSymbol {
                name,
                kind: kind.to_string(),
                signature: Some(signature_from_line(line)),
                start_line: line_no,
                end_line: line_no,
                visibility,
                doc,
            });
        }
    }
    None
}

fn extract_ts_js(content: &str) -> Vec<ExtractedSymbol> {
    let mut out = Vec::new();
    let mut docs = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if let Some(doc) = js_doc(trimmed) {
            docs.push(doc.to_string());
            continue;
        }
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('*') {
            continue;
        }
        if let Some(symbol) = parse_ts_js_decl(trimmed, line_no, take_docs(&mut docs)) {
            out.push(symbol);
        } else {
            docs.clear();
        }
    }
    out
}

fn js_doc(line: &str) -> Option<&str> {
    line.strip_prefix("///")
        .or_else(|| line.strip_prefix("*"))
        .or_else(|| line.strip_prefix("//"))
        .map(|s| s.trim().trim_matches('/').trim_matches('*').trim())
        .filter(|s| !s.is_empty())
}

fn parse_ts_js_decl(line: &str, line_no: usize, doc: Option<String>) -> Option<ExtractedSymbol> {
    let visibility = line.starts_with("export ").then(|| "pub".to_string());
    let s = line.strip_prefix("export default ").or_else(|| line.strip_prefix("export ")).unwrap_or(line);
    let specs = [
        ("async function ", "fn"),
        ("function ", "fn"),
        ("class ", "class"),
        ("interface ", "type"),
        ("type ", "type"),
        ("const ", "const"),
        ("let ", "const"),
    ];
    for (prefix, kind) in specs {
        if let Some(rest) = s.strip_prefix(prefix) {
            let name = symbol_name(rest)?;
            return Some(ExtractedSymbol {
                name,
                kind: kind.to_string(),
                signature: Some(signature_from_line(line)),
                start_line: line_no,
                end_line: line_no,
                visibility,
                doc,
            });
        }
    }
    None
}

fn extract_python(content: &str) -> Vec<ExtractedSymbol> {
    let mut out = Vec::new();
    let mut docs = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if let Some(doc) = trimmed.strip_prefix("#").map(str::trim).filter(|s| !s.is_empty()) {
            docs.push(doc.to_string());
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let (prefix, kind) = if trimmed.starts_with("async def ") {
            ("async def ", "fn")
        } else if trimmed.starts_with("def ") {
            ("def ", "fn")
        } else if trimmed.starts_with("class ") {
            ("class ", "class")
        } else {
            docs.clear();
            continue;
        };
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            if let Some(name) = symbol_name(rest) {
                out.push(ExtractedSymbol {
                    name,
                    kind: kind.to_string(),
                    signature: Some(signature_from_line(trimmed)),
                    start_line: line_no,
                    end_line: line_no,
                    visibility: None,
                    doc: take_docs(&mut docs),
                });
            }
        }
    }
    out
}

fn take_docs(docs: &mut Vec<String>) -> Option<String> {
    if docs.is_empty() {
        None
    } else {
        Some(std::mem::take(docs).join(" "))
    }
}

fn symbol_name(rest: &str) -> Option<String> {
    let name = rest
        .trim()
        .trim_start_matches("r#")
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or_default();
    (!name.is_empty()).then(|| name.to_string())
}

fn signature_from_line(line: &str) -> String {
    let mut out = line.trim().trim_end_matches('{').trim().to_string();
    if out.len() > 240 {
        out.truncate(237);
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_public_symbols() {
        let got = extract(
            "/// Runs it\npub fn run(x: i32) -> i32 { x }\nstruct Hidden;\npub enum Mode { A }\n",
            Some("rust"),
        );
        assert!(got.iter().any(|s| s.name == "run" && s.kind == "fn" && s.visibility.as_deref() == Some("pub")));
        assert!(got.iter().any(|s| s.name == "Hidden" && s.kind == "struct"));
        assert_eq!(got[0].doc.as_deref(), Some("Runs it"));
    }

    #[test]
    fn extracts_typescript_symbols() {
        let got = extract(
            "export function makeThing() {}\nconst localValue = 1;\nexport class Widget {}\n",
            Some("typescript"),
        );
        assert!(got.iter().any(|s| s.name == "makeThing" && s.kind == "fn"));
        assert!(got.iter().any(|s| s.name == "Widget" && s.kind == "class"));
        assert!(got.iter().any(|s| s.name == "localValue" && s.kind == "const"));
    }

    #[test]
    fn extracts_python_symbols() {
        let got = extract("# doc\ndef run():\n    pass\nclass App:\n    pass\n", Some("python"));
        assert!(got.iter().any(|s| s.name == "run" && s.kind == "fn"));
        assert!(got.iter().any(|s| s.name == "App" && s.kind == "class"));
        assert_eq!(got[0].doc.as_deref(), Some("doc"));
    }
}
