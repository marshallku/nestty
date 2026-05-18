//! `nestctl slack` — wrapper over `slack.*` actions.
//!
//! | CLI                                                  | Action                |
//! |------------------------------------------------------|-----------------------|
//! | `slack send <channel> <text> [--thread-ts]`          | `slack.post_message`  |
//! | `slack get <channel> <ts>`                           | `slack.get_message`   |
//! | `slack auth-status`                                  | `slack.auth_status`   |
//!
//! `auth-login` is deliberately NOT a subcommand here — Slack token
//! capture goes through the plugin's own interactive `nestty-plugin-
//! slack auth` binary (env paste + keyring write). Wrapping it in
//! nestctl would invert the trust boundary.

use clap::Subcommand;
use serde_json::{Value, json};

use crate::plugin_cmds::call_and_render;

#[derive(Subcommand, Debug)]
pub enum SlackCommand {
    /// Post a message to a channel (or thread)
    Send {
        /// Channel id (`C…`) or `#name`
        channel: String,
        /// Message text
        text: String,
        /// Reply in the given thread instead of posting top-level
        #[arg(long = "thread-ts")]
        thread_ts: Option<String>,
    },
    /// Fetch a message by `(channel, ts)`
    Get {
        /// Channel id
        channel: String,
        /// Message timestamp (e.g. `1700123456.789012`)
        ts: String,
    },
    /// Print Slack credential / connection status
    AuthStatus,
}

pub fn dispatch(cmd: &SlackCommand, socket_path: &str, json_out: bool) -> i32 {
    match cmd {
        SlackCommand::Send {
            channel,
            text,
            thread_ts,
        } => {
            let mut params = json!({ "channel": channel, "text": text });
            if let Some(ts) = thread_ts {
                params["thread_ts"] = json!(ts);
            }
            call_and_render(socket_path, "slack.post_message", params, json_out, |v| {
                let ts = v.get("ts").and_then(Value::as_str).unwrap_or("?");
                let ch = v.get("channel").and_then(Value::as_str).unwrap_or(channel);
                println!("sent to {ch}: ts={ts}");
            })
        }
        SlackCommand::Get { channel, ts } => call_and_render(
            socket_path,
            "slack.get_message",
            json!({ "channel": channel, "ts": ts }),
            json_out,
            |v| {
                let user = v.get("user").and_then(Value::as_str).unwrap_or("?");
                let text = v.get("text").and_then(Value::as_str).unwrap_or("");
                println!("{user}: {text}");
            },
        ),
        SlackCommand::AuthStatus => call_and_render(
            socket_path,
            "slack.auth_status",
            json!({}),
            json_out,
            render_auth_status,
        ),
    }
}

fn render_auth_status(v: &Value) {
    let configured = v
        .get("configured")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let authenticated = v
        .get("authenticated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let source = v
        .get("credentials_source")
        .and_then(Value::as_str)
        .unwrap_or("none");
    let team_id = v.get("team_id").and_then(Value::as_str).unwrap_or("");
    let user_id = v.get("user_id").and_then(Value::as_str).unwrap_or("");
    let workspace = v.get("workspace").and_then(Value::as_str).unwrap_or("");
    let store_kind = v.get("store_kind").and_then(Value::as_str).unwrap_or("?");
    println!("configured:    {configured}");
    println!("authenticated: {authenticated}");
    println!("source:        {source}");
    if !workspace.is_empty() {
        println!("workspace:     {workspace}");
    }
    if !team_id.is_empty() {
        println!("team_id:       {team_id}");
    }
    if !user_id.is_empty() {
        println!("user_id:       {user_id}");
    }
    println!("store_kind:    {store_kind}");
    if let Some(err) = v.get("fatal_error").and_then(Value::as_str)
        && !err.is_empty()
    {
        println!("error:         {err}");
    }
}
