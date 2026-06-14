//! Model router — content-based model-tier selector.
//!
//! Where [`crate::router`] picks the *provider* (Claude vs DeepSeek) based on
//! live **system load**, this module picks the *model tier* based on the
//! **content of the request**: a trivial FAQ should not burn an Opus call, and
//! an architecture audit should not be answered by a tiny local model.
//!
//! The pipeline mirrors the design the user sketched:
//!
//! ```text
//! prompt → classify() → RequestClass → select_model() → Model (+ scores + reason)
//! ```
//!
//! # Why deterministic (no per-prompt LLM call)
//!
//! Classification runs on the hot path — once per user prompt, inside the
//! `UserPromptSubmit` hook. Calling an LLM here would add latency and cost to
//! *every* request, contradicting the engine's goals (low latency, zero
//! friction, lower cost). So [`classify`] is a fully deterministic heuristic,
//! exactly like [`crate::decision`] and [`crate::router`]. The [`RequestClass`]
//! seam is public, so an LLM-backed classifier can be slotted in later without
//! touching [`select_model`].
//!
//! # Scoring (vs fixed if/else rules)
//!
//! Selection is **score-based**: every model accumulates points from the
//! request's criticality, complexity, type and code-presence, and the highest
//! score wins. This is more expressive than a fixed decision tree (multiple
//! weak signals can combine) and trivially tunable by editing the weight
//! constants below. Ties break toward [`Model::Sonnet`] — the balanced default.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::ModelRouterConfig;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A selectable model tier. `as_str` values match the keys a caller would use
/// to look the model up in `providers.json` / route to an actual backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Model {
    /// Deepest reasoning — architecture, audits, critical decisions.
    Opus,
    /// Balanced default — complex coding, medium structured text.
    Sonnet,
    /// Practical code-focused model — simple/cheap coding tasks.
    DeepSeek,
    /// Fast and cheap — short text generation.
    Haiku,
    /// On-device model — direct FAQ / lookups, zero API cost.
    Local,
}

impl Model {
    /// Lowercase identifier (`"opus"`, `"sonnet"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Model::Opus => "opus",
            Model::Sonnet => "sonnet",
            Model::DeepSeek => "deepseek",
            Model::Haiku => "haiku",
            Model::Local => "local",
        }
    }

    /// Tie-break preference: lower wins when two models share the top score.
    /// Sonnet is the balanced default, mirroring the user's `selectModel`
    /// fallback of `"sonnet"`.
    fn tie_break_rank(self) -> u8 {
        match self {
            Model::Sonnet => 0,
            Model::Opus => 1,
            Model::DeepSeek => 2,
            Model::Haiku => 3,
            Model::Local => 4,
        }
    }

    /// All models, in tie-break order — the canonical iteration order.
    const ALL: [Model; 5] = [
        Model::Sonnet,
        Model::Opus,
        Model::DeepSeek,
        Model::Haiku,
        Model::Local,
    ];
}

impl std::fmt::Display for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Coarse request category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestType {
    /// Simple, direct question.
    Faq,
    /// Programming-related (implement, debug, refactor, code present).
    Code,
    /// Systems design, planning, audits, trade-offs.
    Architecture,
    /// Writing / content generation.
    Text,
    /// Could not be confidently classified.
    Unknown,
}

impl RequestType {
    pub fn as_str(self) -> &'static str {
        match self {
            RequestType::Faq => "faq",
            RequestType::Code => "code",
            RequestType::Architecture => "architecture",
            RequestType::Text => "text",
            RequestType::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for RequestType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Estimated reasoning depth required.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Complexity {
    Low,
    Medium,
    High,
}

impl Complexity {
    pub fn as_str(self) -> &'static str {
        match self {
            Complexity::Low => "low",
            Complexity::Medium => "medium",
            Complexity::High => "high",
        }
    }
}

impl std::fmt::Display for Complexity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The structured classification of a single request — the JSON contract the
/// user specified (`type`, `complexity`, `has_code`, `is_critical`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestClass {
    #[serde(rename = "type")]
    pub req_type: RequestType,
    pub complexity: Complexity,
    pub has_code: bool,
    pub is_critical: bool,
}

/// The outcome of model routing: the chosen model, every model's score, the
/// classification that produced it, and a human-readable reason.
#[derive(Debug, Clone)]
pub struct ModelDecision {
    /// The selected model.
    pub model: Model,
    /// Per-model scores (sorted by model name for stable output).
    pub scores: BTreeMap<&'static str, i32>,
    /// The classification this decision was based on.
    pub class: RequestClass,
    /// Short explanation of the winning signal(s).
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Scoring weights (tunable in one place)
// ---------------------------------------------------------------------------

mod weight {
    /// Critical requests demand the strongest model. This is a **guardrail**
    /// weight: it is deliberately large enough to dominate any combination of
    /// the other signals, so a critical request always routes to Opus (the
    /// user's rule #1: `if is_critical → opus`). Routing stays score-based —
    /// the score map still explains the decision — but correctness/security
    /// requests get a hard floor on model strength.
    pub const CRITICAL_OPUS: i32 = 100;

    pub const HIGH_OPUS: i32 = 2;
    pub const HIGH_SONNET: i32 = 1;
    pub const MEDIUM_SONNET: i32 = 2;
    pub const LOW_HAIKU: i32 = 1;
    pub const LOW_LOCAL: i32 = 1;
    pub const LOW_DEEPSEEK: i32 = 1;

    pub const ARCH_OPUS: i32 = 3;
    pub const ARCH_SONNET: i32 = 1;
    pub const CODE_LOW_DEEPSEEK: i32 = 2;
    pub const CODE_HIGH_SONNET: i32 = 3;
    pub const CODE_HIGH_DEEPSEEK: i32 = 1;
    pub const FAQ_LOCAL: i32 = 3;
    pub const FAQ_HAIKU: i32 = 1;
    pub const TEXT_LOW_HAIKU: i32 = 3;
    pub const TEXT_MED_SONNET: i32 = 2;
    pub const UNKNOWN_SONNET: i32 = 1;

    /// Presence of code nudges toward code-capable models.
    pub const HAS_CODE_SONNET: i32 = 1;
    pub const HAS_CODE_DEEPSEEK: i32 = 1;
}

// ---------------------------------------------------------------------------
// Classification (deterministic heuristic)
// ---------------------------------------------------------------------------

/// Word count threshold above which a prompt is considered at least "medium".
const LONG_PROMPT_WORDS: usize = 60;
/// Word count threshold below which an unspecific prompt stays "low".
const SHORT_PROMPT_WORDS: usize = 12;

/// Classify a raw prompt into a [`RequestClass`].
///
/// Best-effort and deterministic: it leans on keyword/marker detection. The
/// `extra_critical` slice lets the caller inject domain words (from config)
/// that should always flag a request as critical (e.g. `"payment"`, `"auth"`).
pub fn classify(prompt: &str, extra_critical: &[String]) -> RequestClass {
    let lower = prompt.to_lowercase();
    let words = prompt.split_whitespace().count();

    let has_code = detect_code(prompt, &lower);
    let is_critical = detect_critical(&lower, extra_critical);
    let req_type = detect_type(&lower, has_code);
    let complexity = detect_complexity(&lower, words, req_type, is_critical);

    RequestClass {
        req_type,
        complexity,
        has_code,
        is_critical,
    }
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

/// Heuristic code detection: fenced blocks or a cluster of code-ish markers.
fn detect_code(raw: &str, lower: &str) -> bool {
    if raw.contains("```") {
        return true;
    }
    const MARKERS: &[&str] = &[
        "function ", "=>", "();", " def ", "class ", "import ", "fn ", " let ",
        " const ", "public ", "private ", "#include", "</", "{\n", "});",
        "return ", "console.log", "println!", "->", "::", "git ", "npm ", "cargo ",
    ];
    // Require at least two distinct markers so prose mentioning "return" once
    // is not misclassified as code.
    MARKERS.iter().filter(|m| lower.contains(*m)).count() >= 2
}

/// Critical = high-stakes correctness/security/availability words.
fn detect_critical(lower: &str, extra: &[String]) -> bool {
    const CRITICAL: &[&str] = &[
        "production", "produção", "security", "segurança", "vulnerab",
        "payment", "pagamento", "auth", "password", "senha", "credential",
        "critical", "crítico", "urgent", "urgente", "data loss", "perda de dados",
        "migration", "migração", "delete all", "drop table", "lgpd", "gdpr",
        "compliance", "financ",
    ];
    if contains_any(lower, CRITICAL) {
        return true;
    }
    extra.iter().any(|w| {
        let w = w.trim().to_lowercase();
        !w.is_empty() && lower.contains(&w)
    })
}

/// Type detection in priority order: architecture → code → text → faq → unknown.
fn detect_type(lower: &str, has_code: bool) -> RequestType {
    const ARCH: &[&str] = &[
        "architect", "arquitet", "system design", "design the", "scalab",
        "escalab", "microservice", "infrastructure", "infraestrutura",
        "trade-off", "tradeoff", "high-level", "plan the", "planejar",
        "audit", "auditoria", "throughput", "distributed", "data model",
        "schema design",
    ];
    const CODE: &[&str] = &[
        "bug", "error", "erro", "exception", "stack trace", "stacktrace",
        "implement", "implementar", "refactor", "refatorar", "debug",
        "compile", "compilar", "function", "função", "fix ", "corrigir",
        "unit test", "endpoint", "api ", "regex", "algorithm", "algoritmo",
    ];
    const TEXT: &[&str] = &[
        "write a", "escreve", "escrever", "essay", "redação", "blog",
        "summary", "summarize", "resumo", "resumir", "translate", "traduzir",
        "rewrite", "reescrever", "tweet", "post ", "caption", "legenda",
        "headline", "slogan", "copy ",
    ];
    const FAQ: &[&str] = &[
        "what is", "o que é", "what's", "how do", "how to", "como faço",
        "como ", "when ", "quando ", "where ", "onde ", "which ", "qual ",
        "who ", "quem ", "define", "meaning of", "significado",
    ];

    if contains_any(lower, ARCH) {
        return RequestType::Architecture;
    }
    if has_code || contains_any(lower, CODE) {
        return RequestType::Code;
    }
    if contains_any(lower, TEXT) {
        return RequestType::Text;
    }
    if lower.trim_end().ends_with('?') || contains_any(lower, FAQ) {
        return RequestType::Faq;
    }
    RequestType::Unknown
}

/// Complexity heuristic combining type, length and explicit multi-step markers.
fn detect_complexity(
    lower: &str,
    words: usize,
    req_type: RequestType,
    is_critical: bool,
) -> Complexity {
    // Architecture and critical work is inherently high-effort.
    if req_type == RequestType::Architecture || is_critical {
        return Complexity::High;
    }

    const MULTISTEP: &[&str] = &[
        "step by step", "passo a passo", "multiple", "vários", "várias",
        "and then", "e depois", "migrate", "migrar", "across", "end-to-end",
        "ponta a ponta", "several", "first.*then", "pipeline",
    ];
    let multistep = contains_any(lower, MULTISTEP);

    if words >= LONG_PROMPT_WORDS || multistep {
        return Complexity::High;
    }

    // Short, simple lookups stay low.
    if req_type == RequestType::Faq || words <= SHORT_PROMPT_WORDS {
        return Complexity::Low;
    }

    Complexity::Medium
}

// ---------------------------------------------------------------------------
// Selection (score-based)
// ---------------------------------------------------------------------------

/// Pick a model from a [`RequestClass`] using additive scoring.
///
/// The highest-scoring model wins; ties break toward [`Model::Sonnet`] via
/// [`Model::tie_break_rank`]. A request with no signal at all defaults to
/// Sonnet (the score map will be all-zero and the tie-break picks Sonnet).
pub fn select_model(class: RequestClass) -> ModelDecision {
    use weight as w;

    let mut s = Scores::default();
    let mut reasons: Vec<String> = Vec::new();

    if class.is_critical {
        s.opus += w::CRITICAL_OPUS;
        reasons.push("critical→opus".into());
    }

    match class.complexity {
        Complexity::High => {
            s.opus += w::HIGH_OPUS;
            s.sonnet += w::HIGH_SONNET;
            reasons.push("high_complexity".into());
        }
        Complexity::Medium => {
            s.sonnet += w::MEDIUM_SONNET;
            reasons.push("medium_complexity".into());
        }
        Complexity::Low => {
            s.haiku += w::LOW_HAIKU;
            s.local += w::LOW_LOCAL;
            s.deepseek += w::LOW_DEEPSEEK;
            reasons.push("low_complexity".into());
        }
    }

    match class.req_type {
        RequestType::Architecture => {
            s.opus += w::ARCH_OPUS;
            s.sonnet += w::ARCH_SONNET;
            reasons.push("type:architecture".into());
        }
        RequestType::Code => {
            if class.complexity == Complexity::Low {
                s.deepseek += w::CODE_LOW_DEEPSEEK;
                reasons.push("type:code(simple)".into());
            } else {
                s.sonnet += w::CODE_HIGH_SONNET;
                s.deepseek += w::CODE_HIGH_DEEPSEEK;
                reasons.push("type:code(complex)".into());
            }
        }
        RequestType::Faq => {
            s.local += w::FAQ_LOCAL;
            s.haiku += w::FAQ_HAIKU;
            reasons.push("type:faq".into());
        }
        RequestType::Text => {
            if class.complexity == Complexity::Low {
                s.haiku += w::TEXT_LOW_HAIKU;
                reasons.push("type:text(short)".into());
            } else {
                s.sonnet += w::TEXT_MED_SONNET;
                reasons.push("type:text".into());
            }
        }
        RequestType::Unknown => {
            s.sonnet += w::UNKNOWN_SONNET;
            reasons.push("type:unknown→sonnet".into());
        }
    }

    if class.has_code {
        s.sonnet += w::HAS_CODE_SONNET;
        s.deepseek += w::HAS_CODE_DEEPSEEK;
        reasons.push("has_code".into());
    }

    let scores = s.into_map();
    let model = pick_winner(&scores);

    ModelDecision {
        model,
        scores,
        class,
        reason: reasons.join(", "),
    }
}

/// Classify `prompt` and select a model in one call. Honors `cfg.enabled`
/// only at the caller's discretion; this always returns a decision so callers
/// can display it regardless.
pub fn route(prompt: &str, cfg: &ModelRouterConfig) -> ModelDecision {
    let class = classify(prompt, &cfg.critical_keywords);
    select_model(class)
}

// ---------------------------------------------------------------------------
// Internal scoring helpers
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Scores {
    opus: i32,
    sonnet: i32,
    deepseek: i32,
    haiku: i32,
    local: i32,
}

impl Scores {
    fn into_map(self) -> BTreeMap<&'static str, i32> {
        let mut m = BTreeMap::new();
        m.insert(Model::Opus.as_str(), self.opus);
        m.insert(Model::Sonnet.as_str(), self.sonnet);
        m.insert(Model::DeepSeek.as_str(), self.deepseek);
        m.insert(Model::Haiku.as_str(), self.haiku);
        m.insert(Model::Local.as_str(), self.local);
        m
    }
}

/// Choose the model with the strictly-highest score; on ties prefer the lower
/// [`Model::tie_break_rank`]. Iterating [`Model::ALL`] (already in tie-break
/// order) and keeping the first strict maximum yields exactly that.
fn pick_winner(scores: &BTreeMap<&'static str, i32>) -> Model {
    let mut best = Model::Sonnet;
    let mut best_score = i32::MIN;
    let mut best_rank = u8::MAX;
    for m in Model::ALL {
        let sc = *scores.get(m.as_str()).unwrap_or(&0);
        let rank = m.tie_break_rank();
        if sc > best_score || (sc == best_score && rank < best_rank) {
            best = m;
            best_score = sc;
            best_rank = rank;
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn class(
        req_type: RequestType,
        complexity: Complexity,
        has_code: bool,
        is_critical: bool,
    ) -> RequestClass {
        RequestClass {
            req_type,
            complexity,
            has_code,
            is_critical,
        }
    }

    // ------------------------------------------------------------------
    // select_model — score-based outcomes
    // ------------------------------------------------------------------

    #[test]
    fn critical_routes_to_opus() {
        let d = select_model(class(RequestType::Code, Complexity::Medium, true, true));
        assert_eq!(d.model, Model::Opus);
        assert!(d.reason.contains("critical"), "reason: {}", d.reason);
    }

    #[test]
    fn architecture_routes_to_opus() {
        let d = select_model(class(RequestType::Architecture, Complexity::High, false, false));
        assert_eq!(d.model, Model::Opus);
    }

    #[test]
    fn simple_code_routes_to_deepseek() {
        // low-complexity code: deepseek gets low(1)+code_low(2)+has_code(1)=4
        let d = select_model(class(RequestType::Code, Complexity::Low, true, false));
        assert_eq!(d.model, Model::DeepSeek);
    }

    #[test]
    fn complex_code_routes_to_sonnet() {
        // The user's worked example: type=code, complexity=high, critical=false → SONNET.
        let d = select_model(class(RequestType::Code, Complexity::High, true, false));
        assert_eq!(d.model, Model::Sonnet);
    }

    #[test]
    fn faq_routes_to_local() {
        let d = select_model(class(RequestType::Faq, Complexity::Low, false, false));
        assert_eq!(d.model, Model::Local);
    }

    #[test]
    fn short_text_routes_to_haiku() {
        let d = select_model(class(RequestType::Text, Complexity::Low, false, false));
        assert_eq!(d.model, Model::Haiku);
    }

    #[test]
    fn medium_text_routes_to_sonnet() {
        let d = select_model(class(RequestType::Text, Complexity::Medium, false, false));
        assert_eq!(d.model, Model::Sonnet);
    }

    #[test]
    fn unknown_defaults_to_sonnet() {
        let d = select_model(class(RequestType::Unknown, Complexity::Medium, false, false));
        assert_eq!(d.model, Model::Sonnet);
    }

    #[test]
    fn scores_map_has_all_five_models() {
        let d = select_model(class(RequestType::Code, Complexity::High, true, false));
        assert_eq!(d.scores.len(), 5);
        for k in ["opus", "sonnet", "deepseek", "haiku", "local"] {
            assert!(d.scores.contains_key(k), "missing {k}");
        }
    }

    #[test]
    fn winner_is_the_argmax_of_scores() {
        let d = select_model(class(RequestType::Architecture, Complexity::High, false, true));
        let max = d.scores.values().copied().max().unwrap();
        assert_eq!(*d.scores.get(d.model.as_str()).unwrap(), max);
    }

    // ------------------------------------------------------------------
    // classify — deterministic heuristics
    // ------------------------------------------------------------------

    #[test]
    fn detects_fenced_code() {
        let c = classify("here is code:\n```js\nconst x = 1;\n```", &[]);
        assert!(c.has_code);
        assert_eq!(c.req_type, RequestType::Code);
    }

    #[test]
    fn detects_inline_code_markers() {
        let c = classify("write a function that does => return foo();", &[]);
        assert!(c.has_code);
    }

    #[test]
    fn prose_with_one_marker_is_not_code() {
        let c = classify("Please return to the topic of marketing strategy.", &[]);
        assert!(!c.has_code);
    }

    #[test]
    fn architecture_keyword_wins() {
        let c = classify("Design the architecture for a scalable payments system", &[]);
        assert_eq!(c.req_type, RequestType::Architecture);
        assert_eq!(c.complexity, Complexity::High);
        assert!(c.is_critical, "payments should be critical");
    }

    #[test]
    fn faq_question_mark() {
        let c = classify("What is a monad?", &[]);
        assert_eq!(c.req_type, RequestType::Faq);
        assert_eq!(c.complexity, Complexity::Low);
    }

    #[test]
    fn critical_keyword_detected() {
        let c = classify("fix the production outage now", &[]);
        assert!(c.is_critical);
    }

    #[test]
    fn extra_critical_keywords_from_config() {
        let c = classify("update the billing flow", &["billing".to_string()]);
        assert!(c.is_critical);
    }

    #[test]
    fn long_prompt_is_high_complexity() {
        let long = "word ".repeat(LONG_PROMPT_WORDS + 5);
        let c = classify(&long, &[]);
        assert_eq!(c.complexity, Complexity::High);
    }

    #[test]
    fn text_generation_detected() {
        let c = classify("write a blog post about gardening", &[]);
        assert_eq!(c.req_type, RequestType::Text);
    }

    // ------------------------------------------------------------------
    // route — end-to-end on realistic prompts
    // ------------------------------------------------------------------

    #[test]
    fn route_simple_question_to_local() {
        let cfg = ModelRouterConfig::default();
        let d = route("what is the capital of France?", &cfg);
        assert_eq!(d.model, Model::Local);
    }

    #[test]
    fn route_architecture_prompt_to_opus() {
        let cfg = ModelRouterConfig::default();
        let d = route(
            "Design a fault-tolerant distributed system architecture for our microservices",
            &cfg,
        );
        assert_eq!(d.model, Model::Opus);
    }

    #[test]
    fn route_reason_never_empty() {
        let cfg = ModelRouterConfig::default();
        for p in [
            "what is x?",
            "implement a quicksort fn",
            "write a tweet",
            "design the system architecture",
            "hello there friend",
        ] {
            let d = route(p, &cfg);
            assert!(!d.reason.is_empty(), "empty reason for {p:?}");
        }
    }

    // ------------------------------------------------------------------
    // Display / as_str
    // ------------------------------------------------------------------

    #[test]
    fn model_as_str_roundtrip() {
        assert_eq!(Model::Opus.as_str(), "opus");
        assert_eq!(format!("{}", Model::Local), "local");
    }

    #[test]
    fn class_serializes_with_type_key() {
        let c = class(RequestType::Code, Complexity::High, true, false);
        let v = serde_json::to_value(c).unwrap();
        assert_eq!(v["type"], "code");
        assert_eq!(v["complexity"], "high");
        assert_eq!(v["has_code"], true);
        assert_eq!(v["is_critical"], false);
    }
}
