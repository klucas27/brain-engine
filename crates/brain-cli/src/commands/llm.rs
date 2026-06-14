//! `brain llm` — manage LLM provider availability (rate-limit blocks).
//!
//! Subcommands:
//!   block   <provider>  — mark a provider as rate-limited for `claude_window_hours`
//!   unblock <provider>  — remove an existing block
//!   status              — show which providers are currently blocked

use std::path::Path;

use brain_core::config::{self, GlobalConfig};
use brain_core::llm_state;
use brain_core::paths::GlobalPaths;
use brain_core::Result;

use clap::Subcommand;

// ---------------------------------------------------------------------------
// Subcommand definition
// ---------------------------------------------------------------------------

#[derive(Debug, Subcommand)]
pub enum LlmAction {
    /// Mark a provider as rate-limited; routes future requests to the fallback.
    ///
    /// The block expires automatically after `claude_window_hours` (default 5 h).
    /// This command is also called automatically by the Stop hook when Claude
    /// returns a rate-limit error.
    Block {
        /// Provider to block: `claude` or `deepseek`.
        provider: String,
        /// Override the block duration in hours (defaults to `claude_window_hours` in config).
        #[arg(long)]
        hours: Option<u64>,
    },
    /// Remove a rate-limit block so the provider can be used immediately.
    Unblock {
        /// Provider to unblock: `claude` or `deepseek`.
        provider: String,
    },
    /// Show the current availability state of all LLM providers.
    Status,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run(_root: &Path, json: bool, action: LlmAction) -> Result<()> {
    let global = GlobalPaths::resolve()?;
    let global_cfg: GlobalConfig = config::load_or_default(&global.config_file())?;
    let state_path = global.llm_state_file();

    match action {
        LlmAction::Block { provider, hours } => {
            let window_hours = hours.unwrap_or(global_cfg.decision.claude_window_hours);
            let window_secs = window_hours * 3600;
            llm_state::block(&state_path, &provider, window_secs, "rate_limit")?;

            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "status": "blocked",
                        "provider": provider,
                        "window_hours": window_hours,
                    })
                );
            } else {
                eprintln!(
                    "brain llm: {} blocked for {} h — routing to fallback",
                    provider, window_hours
                );
            }
        }

        LlmAction::Unblock { provider } => {
            llm_state::unblock(&state_path, &provider)?;

            if json {
                println!(
                    "{}",
                    serde_json::json!({ "status": "unblocked", "provider": provider })
                );
            } else {
                eprintln!("brain llm: {} unblocked — will use Claude again", provider);
            }
        }

        LlmAction::Status => {
            let state = llm_state::read(&state_path)?;

            if json {
                // Emit the raw state as JSON.
                println!("{}", serde_json::to_string_pretty(&state)?);
            } else if state.0.is_empty() {
                println!("brain llm: all providers available (no active blocks)");
            } else {
                for (provider, block) in &state.0 {
                    if let Some(rem) = llm_state::secs_remaining(&state, provider) {
                        let mins = rem / 60;
                        let secs = rem % 60;
                        println!(
                            "  {} — BLOCKED for {}m {}s (reason: {})",
                            provider, mins, secs, block.reason
                        );
                    } else {
                        println!("  {} — available (block expired)", provider);
                    }
                }
            }
        }
    }

    Ok(())
}
