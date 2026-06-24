//! Intent-aware locator for action prompts.
//!
//! This is the first, dependency-light version of the knowledge-cache locator:
//! it classifies the prompt deterministically and turns retrieved chunks into a
//! compact list of likely edit targets. Later phases can feed it summaries and
//! symbol maps without changing the JSON contract exposed to clients.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::LocatorConfig;
use crate::retrieve::RetrievedChunk;

/// Coarse user intent for deciding whether to inject edit guidance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IntentKind {
    Action,
    Question,
}

/// Deterministic intent classification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub kind: IntentKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verb: Option<String>,
    pub targets: Vec<String>,
}

/// One ranked file/line target for an action prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocatorTarget {
    pub file: String,
    pub line: usize,
    pub why: String,
    pub score: f32,
}

/// Full locator payload returned by the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocatorReport {
    pub enabled: bool,
    pub inject_directive: bool,
    pub intent: Intent,
    pub targets: Vec<LocatorTarget>,
}

/// Classify prompt intent without network calls or model inference.
pub fn classify_intent(prompt: &str) -> Intent {
    let lower = prompt.to_lowercase();
    let verb = action_verb(&lower).map(str::to_string);
    let kind = if verb.is_some() {
        IntentKind::Action
    } else {
        IntentKind::Question
    };
    let targets = extract_targets(prompt);

    Intent {
        kind,
        verb,
        targets,
    }
}

/// Build a locator report from the prompt and already-retrieved chunks.
pub fn locate(
    prompt: &str,
    chunks: &[RetrievedChunk],
    cfg: &LocatorConfig,
) -> Option<LocatorReport> {
    if !cfg.enabled {
        return None;
    }

    let intent = classify_intent(prompt);
    let targets = if intent.kind == IntentKind::Action {
        rank_targets(chunks, &intent.targets, cfg.max_targets)
    } else {
        Vec::new()
    };

    Some(LocatorReport {
        enabled: true,
        inject_directive: cfg.inject_directive,
        intent,
        targets,
    })
}

fn action_verb(lower: &str) -> Option<&'static str> {
    const VERBS: &[(&str, &str)] = &[
        ("crie", "criar"),
        ("criar", "criar"),
        ("adicione", "adicionar"),
        ("adicionar", "adicionar"),
        ("inclua", "adicionar"),
        ("implemente", "implementar"),
        ("implementar", "implementar"),
        ("edite", "editar"),
        ("editar", "editar"),
        ("altere", "alterar"),
        ("alterar", "alterar"),
        ("mude", "alterar"),
        ("corrija", "corrigir"),
        ("corrigir", "corrigir"),
        ("conserte", "corrigir"),
        ("renomeie", "renomear"),
        ("renomear", "renomear"),
        ("remova", "remover"),
        ("remover", "remover"),
        ("delete", "remover"),
        ("mova", "mover"),
        ("mover", "mover"),
        ("refatore", "refatorar"),
        ("refatorar", "refatorar"),
        ("create", "create"),
        ("add", "add"),
        ("implement", "implement"),
        ("edit", "edit"),
        ("change", "change"),
        ("fix", "fix"),
        ("rename", "rename"),
        ("remove", "remove"),
        ("delete", "delete"),
        ("move", "move"),
        ("refactor", "refactor"),
    ];

    VERBS
        .iter()
        .find_map(|(needle, canonical)| contains_word(lower, needle).then_some(*canonical))
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    haystack
        .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
        .any(|w| w == needle)
}

fn extract_targets(prompt: &str) -> Vec<String> {
    let mut out = Vec::new();

    for quoted in quoted_spans(prompt) {
        push_target(&mut out, quoted);
    }

    for raw in prompt.split_whitespace() {
        let token = raw.trim_matches(|c: char| {
            matches!(
                c,
                '.' | ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '`'
            )
        });
        if token.len() < 3 {
            continue;
        }
        if looks_like_file(token) || looks_like_symbol(token) || looks_like_feature_word(token) {
            push_target(&mut out, token.to_string());
        }
    }

    out
}

fn quoted_spans(prompt: &str) -> Vec<String> {
    let mut spans = Vec::new();
    let mut chars = prompt.char_indices().peekable();
    while let Some((start, ch)) = chars.next() {
        if ch != '`' && ch != '"' && ch != '\'' {
            continue;
        }
        for (end, next) in chars.by_ref() {
            if next == ch {
                let span = prompt[start + ch.len_utf8()..end].trim();
                if !span.is_empty() {
                    spans.push(span.to_string());
                }
                break;
            }
        }
    }
    spans
}

fn looks_like_file(token: &str) -> bool {
    token.contains('/')
        || [
            ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".kt", ".swift", ".rb",
            ".php", ".css", ".scss", ".html", ".md", ".json", ".toml", ".yaml", ".yml",
        ]
        .iter()
        .any(|ext| token.ends_with(ext))
}

fn looks_like_symbol(token: &str) -> bool {
    token.contains("::")
        || token.contains('.')
        || token.contains('_')
        || token.chars().any(|c| c.is_ascii_uppercase())
}

fn looks_like_feature_word(token: &str) -> bool {
    const STOP: &[&str] = &[
        "crie",
        "criar",
        "adicione",
        "adicionar",
        "implemente",
        "implementar",
        "edite",
        "editar",
        "corrija",
        "corrigir",
        "remova",
        "remover",
        "create",
        "add",
        "implement",
        "edit",
        "fix",
        "remove",
        "uma",
        "um",
        "the",
        "para",
        "com",
        "sem",
        "from",
        "into",
    ];
    token.len() >= 4 && !STOP.iter().any(|w| token.eq_ignore_ascii_case(w))
}

fn push_target(out: &mut Vec<String>, value: String) {
    let value = value.trim().to_string();
    if value.is_empty() || out.iter().any(|v| v.eq_ignore_ascii_case(&value)) {
        return;
    }
    if out.len() < 12 {
        out.push(value);
    }
}

#[derive(Debug)]
struct Candidate {
    file: String,
    line: usize,
    why_terms: Vec<String>,
    score: f32,
}

fn rank_targets(
    chunks: &[RetrievedChunk],
    prompt_targets: &[String],
    max_targets: usize,
) -> Vec<LocatorTarget> {
    if max_targets == 0 {
        return Vec::new();
    }

    let mut by_file: BTreeMap<String, Candidate> = BTreeMap::new();
    for chunk in chunks {
        let matched = matched_terms(chunk, prompt_targets);
        let lexical_boost = matched.len() as f32 * 0.25;
        let score = chunk.score + lexical_boost;
        let entry = by_file
            .entry(chunk.file_path.clone())
            .or_insert_with(|| Candidate {
                file: chunk.file_path.clone(),
                line: chunk.start_line,
                why_terms: Vec::new(),
                score,
            });

        if score > entry.score {
            entry.line = chunk.start_line;
            entry.score = score;
        }
        for term in matched {
            if !entry
                .why_terms
                .iter()
                .any(|t| t.eq_ignore_ascii_case(&term))
            {
                entry.why_terms.push(term);
            }
        }
    }

    let mut candidates: Vec<_> = by_file.into_values().collect();
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.file.cmp(&b.file))
    });

    candidates
        .into_iter()
        .take(max_targets)
        .map(|c| LocatorTarget {
            file: c.file,
            line: c.line,
            why: if c.why_terms.is_empty() {
                "semantic match from current index".to_string()
            } else {
                format!("matches {}", c.why_terms.join(", "))
            },
            score: c.score,
        })
        .collect()
}

fn matched_terms(chunk: &RetrievedChunk, prompt_targets: &[String]) -> Vec<String> {
    let path = chunk.file_path.to_lowercase();
    let content = chunk.content.to_lowercase();
    prompt_targets
        .iter()
        .filter_map(|target| {
            let t = target.to_lowercase();
            if t.len() >= 3 && (path.contains(&t) || content.contains(&t)) {
                Some(target.clone())
            } else {
                None
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(file_path: &str, content: &str, score: f32) -> RetrievedChunk {
        RetrievedChunk {
            chunk_id: 1,
            file_path: file_path.to_string(),
            start_line: 12,
            end_line: 30,
            content: content.to_string(),
            score,
            token_estimate: 10,
        }
    }

    #[test]
    fn action_prompt_is_classified_as_action() {
        let intent = classify_intent("crie um KPI valor total na aba dashboard");
        assert_eq!(intent.kind, IntentKind::Action);
        assert_eq!(intent.verb.as_deref(), Some("criar"));
        assert!(intent.targets.iter().any(|t| t.eq_ignore_ascii_case("KPI")));
        assert!(intent
            .targets
            .iter()
            .any(|t| t.eq_ignore_ascii_case("dashboard")));
    }

    #[test]
    fn question_prompt_is_classified_as_question() {
        let intent = classify_intent("onde fica o cache de respostas?");
        assert_eq!(intent.kind, IntentKind::Question);
        assert!(intent.verb.is_none());
    }

    #[test]
    fn action_locator_returns_ranked_file_targets() {
        let cfg = LocatorConfig::default();
        let chunks = vec![
            chunk(
                "src/dashboard/KpiGrid.tsx",
                "render KPI cards for dashboard",
                0.80,
            ),
            chunk("src/auth/login.ts", "login form", 0.90),
        ];

        let report = locate("adicione KPI valor total no dashboard", &chunks, &cfg).unwrap();

        assert_eq!(report.intent.kind, IntentKind::Action);
        assert_eq!(report.targets[0].file, "src/dashboard/KpiGrid.tsx");
        assert!(report.targets[0].why.contains("KPI"));
    }
}
