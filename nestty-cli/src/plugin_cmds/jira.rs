//! `nestctl jira` — ergonomic wrapper over the `jira.*` action surface.
//!
//! Maps clap subcommands onto actions exposed by `nestty-plugin-jira`:
//!
//! | CLI                                       | Action                  |
//! |-------------------------------------------|-------------------------|
//! | `jira mine [--status S] [--project P]`    | `jira.list_my_tickets`  |
//! | `jira ticket <key>`                       | `jira.get_ticket`       |
//! | `jira transition <key> <status>`          | `jira.transition`       |
//! | `jira comment <key> <text>`               | `jira.add_comment`      |
//! | `jira auth-status`                        | `jira.auth_status`      |
//!
//! Every subcommand renders a focused human view by default and the
//! raw action payload under `--json`. No new IPC, no plugin-side work.

use clap::Subcommand;
use serde_json::{Value, json};

use crate::plugin_cmds::call_and_render;

#[derive(Subcommand, Debug)]
pub enum JiraCommand {
    /// List tickets assigned to the current user, newest-updated first
    Mine {
        /// Filter by status (e.g. `In Progress`, `Done`)
        #[arg(long)]
        status: Option<String>,
        /// Filter by project key
        #[arg(long)]
        project: Option<String>,
        /// JQL date expression for `updated >` (e.g. `-7d`, `2026-01-01`)
        #[arg(long)]
        since: Option<String>,
    },
    /// Show the full Jira payload for a single ticket
    Ticket {
        /// Issue key (e.g. `PROJ-123`)
        key: String,
    },
    /// Move a ticket to the given status
    Transition {
        /// Issue key
        key: String,
        /// Target status name (case-sensitive Jira value)
        status: String,
    },
    /// Add a comment to a ticket
    Comment {
        /// Issue key
        key: String,
        /// Comment body
        text: String,
    },
    /// Print Jira credential / connection status
    AuthStatus,
}

pub fn dispatch(cmd: &JiraCommand, socket_path: &str, json_out: bool) -> i32 {
    match cmd {
        JiraCommand::Mine {
            status,
            project,
            since,
        } => {
            let mut params = json!({});
            if let Some(s) = status {
                params["status"] = json!(s);
            }
            if let Some(p) = project {
                params["project"] = json!(p);
            }
            if let Some(s) = since {
                params["updated_since"] = json!(s);
            }
            call_and_render(
                socket_path,
                "jira.list_my_tickets",
                params,
                json_out,
                render_mine,
            )
        }
        JiraCommand::Ticket { key } => call_and_render(
            socket_path,
            "jira.get_ticket",
            json!({ "key": key }),
            json_out,
            render_ticket,
        ),
        JiraCommand::Transition { key, status } => call_and_render(
            socket_path,
            "jira.transition",
            json!({ "key": key, "status": status }),
            json_out,
            render_transition,
        ),
        JiraCommand::Comment { key, text } => call_and_render(
            socket_path,
            "jira.add_comment",
            json!({ "key": key, "body": text }),
            json_out,
            |v| {
                let id = v.get("comment_id").and_then(Value::as_str).unwrap_or("?");
                println!("commented on {key}: comment_id={id}");
            },
        ),
        JiraCommand::AuthStatus => call_and_render(
            socket_path,
            "jira.auth_status",
            json!({}),
            json_out,
            render_auth_status,
        ),
    }
}

fn render_mine(v: &Value) {
    let tickets = v
        .get("tickets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if tickets.is_empty() {
        println!("(no tickets)");
    } else {
        let key_w = tickets
            .iter()
            .map(|t| t.get("key").and_then(Value::as_str).unwrap_or("").len())
            .max()
            .unwrap_or(0)
            .max(3);
        let status_w = tickets
            .iter()
            .map(|t| t.get("status").and_then(Value::as_str).unwrap_or("").len())
            .max()
            .unwrap_or(0)
            .max(6);
        for t in &tickets {
            let key = t.get("key").and_then(Value::as_str).unwrap_or("?");
            let status = t.get("status").and_then(Value::as_str).unwrap_or("?");
            let summary = t.get("summary").and_then(Value::as_str).unwrap_or("");
            println!("{key:<key_w$}  {status:<status_w$}  {summary}");
        }
    }
    if v.get("truncated").and_then(Value::as_bool).unwrap_or(false) {
        println!("(truncated — more pages exist; narrow with --status / --project / --since)");
    }
}

fn render_ticket(v: &Value) {
    // The action returns the verbatim Jira issue payload; pluck the
    // most useful fields out and dump the rest after.
    let key = v.get("key").and_then(Value::as_str).unwrap_or("?");
    let fields = v.get("fields");
    let summary = fields
        .and_then(|f| f.get("summary"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let status = fields
        .and_then(|f| f.get("status"))
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let assignee = fields
        .and_then(|f| f.get("assignee"))
        .and_then(|a| a.get("displayName"))
        .and_then(Value::as_str)
        .unwrap_or("(unassigned)");
    let updated = fields
        .and_then(|f| f.get("updated"))
        .and_then(Value::as_str)
        .unwrap_or("");
    println!("{key} — {summary}");
    println!("status:   {status}");
    println!("assignee: {assignee}");
    if !updated.is_empty() {
        println!("updated:  {updated}");
    }
    let description = fields
        .and_then(|f| f.get("description"))
        .and_then(|d| d.get("content"))
        .map(adf_first_paragraph);
    if let Some(text) = description
        && !text.is_empty()
    {
        println!();
        println!("{text}");
    }
}

/// Pull the first paragraph of an ADF (Atlassian Document Format) body
/// as plain text. The plugin itself owns full ADF rendering for trigger
/// interpolation; the CLI keeps a tiny inline reader because pulling
/// `nestty-plugin-jira` as a dep just for one render path would invert
/// the crate ownership.
fn adf_first_paragraph(content: &Value) -> String {
    let Some(arr) = content.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for node in arr {
        if node.get("type").and_then(Value::as_str) != Some("paragraph") {
            continue;
        }
        if let Some(children) = node.get("content").and_then(Value::as_array) {
            for ch in children {
                if let Some(t) = ch.get("text").and_then(Value::as_str) {
                    out.push_str(t);
                }
            }
        }
        break;
    }
    out
}

fn render_transition(v: &Value) {
    let key = v.get("key").and_then(Value::as_str).unwrap_or("?");
    let from = v.get("from_status").and_then(Value::as_str).unwrap_or("?");
    let to = v.get("to_status").and_then(Value::as_str).unwrap_or("?");
    println!("{key}: {from} → {to}");
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
    let display_name = v.get("display_name").and_then(Value::as_str).unwrap_or("");
    let account_id = v.get("account_id").and_then(Value::as_str).unwrap_or("");
    let base_url = v.get("base_url").and_then(Value::as_str).unwrap_or("");
    let workspace = v.get("workspace").and_then(Value::as_str).unwrap_or("");
    let store_kind = v.get("store_kind").and_then(Value::as_str).unwrap_or("?");
    println!("configured:    {configured}");
    println!("authenticated: {authenticated}");
    println!("source:        {source}");
    if !base_url.is_empty() {
        println!("base_url:      {base_url}");
    }
    if !workspace.is_empty() {
        println!("workspace:     {workspace}");
    }
    if !display_name.is_empty() {
        println!("display_name:  {display_name}");
    }
    if !account_id.is_empty() {
        println!("account_id:    {account_id}");
    }
    println!("store_kind:    {store_kind}");
    if let Some(err) = v.get("fatal_error").and_then(Value::as_str)
        && !err.is_empty()
    {
        println!("error:         {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adf_first_paragraph_extracts_inline_text() {
        let body = json!([
            {
                "type": "paragraph",
                "content": [
                    { "type": "text", "text": "hello " },
                    { "type": "text", "text": "world" }
                ]
            },
            {
                "type": "paragraph",
                "content": [{ "type": "text", "text": "later" }]
            }
        ]);
        assert_eq!(adf_first_paragraph(&body), "hello world");
    }

    #[test]
    fn adf_first_paragraph_skips_non_paragraph_top_level() {
        let body = json!([
            { "type": "heading", "content": [{ "type": "text", "text": "hi" }] },
            {
                "type": "paragraph",
                "content": [{ "type": "text", "text": "body" }]
            }
        ]);
        assert_eq!(adf_first_paragraph(&body), "body");
    }

    #[test]
    fn adf_first_paragraph_empty_on_missing_text() {
        let body = json!([
            { "type": "paragraph", "content": [] }
        ]);
        assert_eq!(adf_first_paragraph(&body), "");
    }
}
