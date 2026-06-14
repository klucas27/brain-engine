//! `brain route` — classify a prompt and show which model the router picks.
//!
//! This exposes the content-based model router ([`brain_core::model_router`])
//! directly so users can inspect and tune the classification + scoring without
//! going through Claude Code. It runs entirely locally and makes no API calls.

use std::path::Path;

use brain_core::config::{self, GlobalConfig};
use brain_core::model_router;
use brain_core::paths::GlobalPaths;
use brain_core::Result;

/// Execute `brain route "<prompt>"`.
pub fn run(_root: &Path, prompt: &str, json: bool) -> Result<()> {
    let global = GlobalPaths::resolve()?;
    let global_cfg: GlobalConfig = config::load_or_default(&global.config_file())?;
    let decision = model_router::route(prompt, &global_cfg.model_router);

    if json {
        let value = serde_json::json!({
            "selected_model": decision.model.as_str(),
            "classification": decision.class,
            "scores": decision.scores,
            "reason": decision.reason,
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        let c = &decision.class;
        println!("[MODEL ROUTER]");
        println!("  Type:           {}", c.req_type);
        println!("  Complexity:     {}", c.complexity);
        println!("  Has code:       {}", c.has_code);
        println!("  Critical:       {}", c.is_critical);
        println!("  Selected Model: {}", decision.model.as_str().to_uppercase());
        println!("  Reason:         {}", decision.reason);
        // Score breakdown, highest first.
        let mut scored: Vec<(&&str, &i32)> = decision.scores.iter().collect();
        scored.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        let line = scored
            .iter()
            .map(|(m, s)| format!("{m}={s}"))
            .collect::<Vec<_>>()
            .join("  ");
        println!("  Scores:         {line}");
    }

    Ok(())
}
