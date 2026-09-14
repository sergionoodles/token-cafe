//! tc-probe: one-shot usage probes for Token Cafe (Noctalia 5).
//!
//! Called by service.luau and prints a single JSON object to stdout:
//!   tc-probe <codex|opencode|grok> --cli-bin <bin> [--timeout <secs>]
//!   tc-probe antigravity --state-db <path> [--project-id <id>] [--timeout <secs>]

mod antigravity;
mod codex;
mod grok;
mod opencode;
mod rpc;
mod util;

use std::time::{Duration, Instant};
use util::{clean_error, configure_user_path, error_result};

fn usage_error(provider: &str, msg: &str) -> ! {
    print!("{}", error_result(provider, msg));
    std::process::exit(0);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let provider = args.first().map(|s| s.as_str()).unwrap_or("");
    if !["codex", "opencode", "grok", "antigravity"].contains(&provider) {
        usage_error("unknown", "Provider must be codex, opencode, grok, or antigravity");
    }
    let mut cli_bin = String::new();
    let mut state_db = String::new();
    let mut project_id = String::new();
    let mut timeout_s = 25.0;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--cli-bin" => {
                i += 1;
                cli_bin = args.get(i).cloned().unwrap_or_default();
            }
            "--state-db" => {
                i += 1;
                state_db = args.get(i).cloned().unwrap_or_default();
            }
            "--project-id" => {
                i += 1;
                project_id = args.get(i).cloned().unwrap_or_default();
            }
            "--timeout" => {
                i += 1;
                timeout_s = args.get(i).and_then(|v| v.parse().ok()).unwrap_or(25.0);
            }
            other => usage_error(provider, &format!("Unknown argument: {other}")),
        }
        i += 1;
    }
    if timeout_s <= 0.0 {
        usage_error(provider, "Timeout must be positive");
    }
    let timeout = Duration::from_secs_f64(timeout_s);
    let deadline = Instant::now() + timeout;
    configure_user_path(timeout.min(Duration::from_secs(3)));

    let defaults = [("codex", "codex"), ("opencode", "opencode"), ("grok", "grok")];
    let output = match provider {
        "antigravity" => {
            if state_db.is_empty() {
                usage_error(provider, "--state-db is required");
            }
            antigravity::probe(&state_db, &project_id, deadline)
        }
        name => {
            if cli_bin.is_empty() {
                cli_bin = defaults.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string()).unwrap_or_default();
            }
            match name {
                "codex" => codex::probe(&cli_bin, deadline),
                "opencode" => opencode::probe(&cli_bin, deadline),
                "grok" => grok::probe(&cli_bin, deadline),
                _ => error_result(name, clean_error("unknown provider")),
            }
        }
    };
    print!("{}", serde_json::to_string(&output).unwrap_or_else(|_| "{\"ready\":false}".to_string()));
}
