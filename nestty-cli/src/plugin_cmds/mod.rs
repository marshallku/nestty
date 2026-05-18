//! Per-plugin ergonomic CLI subcommands (Phase 19.1).
//!
//! Each module here exposes a clap `Subcommand` enum + a `dispatch`
//! entrypoint that builds the right action params, optionally
//! preflight-resolves user-supplied id shorthands, calls the action
//! via the shared socket client, and renders the response — either
//! as JSON (`--json`) or in a human-readable form tailored to the
//! subcommand. Every subcommand is otherwise just sugar over the
//! generic `nestctl call <action> --params '{...}'` path; no new
//! IPC, no new actions, no plugin-side work.
//!
//! Slice 19.1a ships `todo`. 19.1b adds `kb` / `calendar`. 19.1c adds
//! `slack` / `git`. `jira` waits on Phase 16. Per-module structure
//! lets each surface evolve independently — naming conventions,
//! prefix-resolution rules, output formats — without bloating
//! `commands.rs`.

pub mod bookmark;
pub mod calendar;
pub mod context;
pub mod git;
pub mod jira;
pub mod slack;
pub mod todo;

use serde_json::Value;

use crate::client;

/// Shared call+render. `json_out` dumps `result` as pretty JSON;
/// otherwise the `human` callback renders. Returns 0 on success,
/// 1 on transport error / `ok: false`.
pub fn call_and_render(
    socket_path: &str,
    method: &str,
    params: Value,
    json_out: bool,
    human: impl FnOnce(&Value),
) -> i32 {
    let resp = match client::send_command(socket_path, method, params) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to connect: {e}");
            return 1;
        }
    };
    if !resp.ok {
        if let Some(err) = resp.error {
            eprintln!("Error [{}]: {}", err.code, err.message);
        }
        return 1;
    }
    let result = resp.result.unwrap_or(Value::Null);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&result).unwrap());
    } else {
        human(&result);
    }
    0
}
