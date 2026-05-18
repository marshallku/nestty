//! `nestctl calendar` — wrapper over `calendar.*` actions.
//!
//! | CLI                                  | Action                  |
//! |--------------------------------------|-------------------------|
//! | `calendar today`                     | `calendar.list_events`  |
//! | `calendar next [--within Nh]`        | `calendar.list_events`  |
//! | `calendar event <id>`                | `calendar.event_details`|
//! | `calendar auth-status`               | `calendar.auth_status`  |

use chrono::{DateTime, Local, NaiveDate};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::plugin_cmds::call_and_render;

const DEFAULT_NEXT_WITHIN_HOURS: u32 = 2;
const TODAY_LOOKAHEAD_HOURS: u32 = 24;

#[derive(Subcommand, Debug)]
pub enum CalendarCommand {
    /// Today's remaining events (local-date filtered, 24h lookahead)
    Today,
    /// Events within the next N hours
    Next {
        /// Hours to look ahead (default 2)
        #[arg(long, default_value_t = DEFAULT_NEXT_WITHIN_HOURS)]
        within: u32,
    },
    /// Show full details for a single event
    Event {
        /// Event id (from `today` / `next` output)
        id: String,
    },
    /// Print Google Calendar credential / connection status
    AuthStatus,
}

pub fn dispatch(cmd: &CalendarCommand, socket_path: &str, json_out: bool) -> i32 {
    match cmd {
        CalendarCommand::Today => {
            let today = Local::now().date_naive();
            call_and_render(
                socket_path,
                "calendar.list_events",
                json!({ "lookahead_hours": TODAY_LOOKAHEAD_HOURS }),
                json_out,
                |v| render_events(v, Some(today)),
            )
        }
        CalendarCommand::Next { within } => call_and_render(
            socket_path,
            "calendar.list_events",
            json!({ "lookahead_hours": within }),
            json_out,
            |v| render_events(v, None),
        ),
        CalendarCommand::Event { id } => call_and_render(
            socket_path,
            "calendar.event_details",
            json!({ "id": id }),
            json_out,
            render_event_details,
        ),
        CalendarCommand::AuthStatus => call_and_render(
            socket_path,
            "calendar.auth_status",
            json!({}),
            json_out,
            render_auth_status,
        ),
    }
}

/// Filter to `today_filter` (local date) when provided — used by the
/// `today` subcommand which asks for a 24h window and trims client-
/// side so events past midnight don't bleed in.
fn render_events(v: &Value, today_filter: Option<NaiveDate>) {
    let events: Vec<&Value> = v
        .get("events")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|e| {
                    let Some(filter) = today_filter else {
                        return true;
                    };
                    let Some(start) = e.get("start_time").and_then(Value::as_str) else {
                        return false;
                    };
                    DateTime::parse_from_rfc3339(start)
                        .map(|dt| dt.with_timezone(&Local).date_naive() == filter)
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    if events.is_empty() {
        println!("(no events)");
        return;
    }
    for e in events {
        let title = e
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("(no title)");
        let when = format_event_when(e);
        println!("{when}  {title}");
        if let Some(loc) = e.get("location").and_then(Value::as_str)
            && !loc.is_empty()
        {
            println!("        @ {loc}");
        }
    }
}

fn render_event_details(v: &Value) {
    let title = v
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("(no title)");
    println!("{title}");
    println!("when:     {}", format_event_when(v));
    if let Some(loc) = v.get("location").and_then(Value::as_str)
        && !loc.is_empty()
    {
        println!("location: {loc}");
    }
    if let Some(url) = v.get("conference_url").and_then(Value::as_str)
        && !url.is_empty()
    {
        println!("meeting:  {url}");
    }
    if let Some(desc) = v.get("description").and_then(Value::as_str)
        && !desc.is_empty()
    {
        println!();
        println!("{desc}");
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
    let account = v.get("account").and_then(Value::as_str).unwrap_or("");
    let store_kind = v.get("store_kind").and_then(Value::as_str).unwrap_or("?");
    println!("configured:    {configured}");
    println!("authenticated: {authenticated}");
    if !account.is_empty() {
        println!("account:       {account}");
    }
    println!("store_kind:    {store_kind}");
    if let Some(err) = v.get("fatal_error").and_then(Value::as_str)
        && !err.is_empty()
    {
        println!("error:         {err}");
    }
}

fn format_event_when(e: &Value) -> String {
    let all_day = e.get("all_day").and_then(Value::as_bool).unwrap_or(false);
    let start = e
        .get("start_time")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Local));
    let end = e
        .get("end_time")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Local));
    match (start, end, all_day) {
        (Some(s), _, true) => s.format("%Y-%m-%d (all-day)").to_string(),
        (Some(s), Some(e), _) => {
            if s.date_naive() == e.date_naive() {
                format!("{} → {}", s.format("%Y-%m-%d %H:%M"), e.format("%H:%M"))
            } else {
                format!(
                    "{} → {}",
                    s.format("%Y-%m-%d %H:%M"),
                    e.format("%Y-%m-%d %H:%M")
                )
            }
        }
        (Some(s), None, _) => s.format("%Y-%m-%d %H:%M").to_string(),
        _ => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_event_when_same_day_renders_compact_range() {
        let e = json!({
            "start_time": "2026-05-15T13:00:00+09:00",
            "end_time":   "2026-05-15T14:00:00+09:00",
            "all_day":    false,
        });
        let s = format_event_when(&e);
        assert!(s.contains("→"));
        assert!(s.contains("2026-05-15"));
    }

    #[test]
    fn format_event_when_all_day_shows_no_time() {
        let e = json!({
            "start_time": "2026-05-15T00:00:00+09:00",
            "end_time":   "2026-05-16T00:00:00+09:00",
            "all_day":    true,
        });
        assert!(format_event_when(&e).contains("all-day"));
    }
}
