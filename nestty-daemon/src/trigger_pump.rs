//! Per-tick drain targets shared by the daemon's trigger thread and the
//! config hot-reload path. Ported from `nestty-linux/src/window.rs` so
//! the daemon can host TriggerEngine + ContextService independently of
//! the GTK timer that historically drove the pump.
//!
//! Identity with the GUI version is intentional: the GUI's copy stays in
//! place during the cut-over (env-gated by `NESTTYD_HOST_TRIGGERS`) so a
//! rollback is one env flip. A follow-up step removes the GUI copy.

use std::collections::HashMap;
use std::sync::Arc;

use nestty_core::context::ContextService;
use nestty_core::event_bus::{Event as BusEvent, EventBus as CoreEventBus, EventReceiver};
use nestty_core::trigger::{Trigger, TriggerEngine, covering_patterns};

/// Three context-driving receivers + the dynamic trigger subscription
/// set. The pump drains context first (so `{context.*}` interpolation
/// reflects the latest panel/cwd), then triggers.
pub struct PumpState {
    ctx_focused: EventReceiver,
    ctx_exited: EventReceiver,
    ctx_cwd: EventReceiver,
    trigger_subs: TriggerSubscriptions,
}

impl PumpState {
    pub fn new(bus: &Arc<CoreEventBus>) -> Self {
        Self {
            ctx_focused: bus.subscribe("panel.focused"),
            ctx_exited: bus.subscribe("panel.exited"),
            ctx_cwd: bus.subscribe("terminal.cwd_changed"),
            trigger_subs: TriggerSubscriptions::new(),
        }
    }

    pub fn drain_context_only(&self, ctx: &ContextService) {
        while let Some(event) = self.ctx_focused.try_recv() {
            ctx.apply_event(&event);
        }
        while let Some(event) = self.ctx_exited.try_recv() {
            ctx.apply_event(&event);
        }
        while let Some(event) = self.ctx_cwd.try_recv() {
            ctx.apply_event(&event);
        }
    }

    /// Context first (so `{context.*}` interpolation is fresh), then
    /// triggers. One context snapshot per dispatched event.
    pub fn pump_all(&self, ctx: &ContextService, engine: &TriggerEngine) {
        self.drain_context_only(ctx);
        self.trigger_subs.drain_into(|event| {
            let snap = ctx.snapshot();
            engine.dispatch(&event, Some(&snap));
        });
    }

    pub fn reconcile_triggers(&mut self, bus: &Arc<CoreEventBus>, triggers: &[Trigger]) {
        self.trigger_subs.reconcile(bus, triggers);
    }

    pub fn trigger_subs_len(&self) -> usize {
        self.trigger_subs.len()
    }
}

/// One bus receiver per unique `event_kind` pattern across active triggers.
/// Reconciled at startup and on hot reload: still-needed patterns keep
/// their existing receivers (so pending events survive unrelated reloads),
/// removed patterns drop (queues GC'd on next publish).
pub struct TriggerSubscriptions {
    receivers: HashMap<String, EventReceiver>,
}

impl TriggerSubscriptions {
    pub fn new() -> Self {
        Self {
            receivers: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.receivers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receivers.is_empty()
    }

    /// Reduces requested patterns via `covering_patterns` first so
    /// overlap (`*` plus `panel.focused`) collapses to a single
    /// broader receiver — otherwise duplicate delivery would
    /// double-fire side effects. New patterns get fresh
    /// `subscribe_unbounded` receivers.
    pub fn reconcile(&mut self, bus: &Arc<CoreEventBus>, triggers: &[Trigger]) {
        // Three flavors of event must be subscribed per trigger:
        // (1) `when.event_kind`, (2) `await.event_kind`,
        // (3) `<action>.completed`/`.failed` for await-bearing triggers.
        // Missing #3 degrades await to "registers preflight, never
        // promotes" — see docs/workflow-runtime.md "Async correlation".
        let mut raw: Vec<String> = Vec::with_capacity(triggers.len() * 3);
        for t in triggers {
            raw.push(t.when.event_kind.clone());
            if let Some(aw) = &t.r#await {
                raw.push(aw.event_kind.clone());
                raw.push(format!("{}.completed", t.action));
                raw.push(format!("{}.failed", t.action));
            }
        }
        let needed: std::collections::HashSet<String> =
            covering_patterns(raw).into_iter().collect();
        self.receivers.retain(|pattern, _| needed.contains(pattern));
        for pattern in needed {
            self.receivers
                .entry(pattern.clone())
                .or_insert_with(|| bus.subscribe_unbounded(pattern.clone()));
        }
    }

    /// Drain every receiver fully. Ordering invariant: registry-sourced
    /// `<X>.completed`/`.failed` events must run BEFORE `await.event_kind`
    /// in the same tick — otherwise a same-tick awaited reply hits a
    /// preflight that hasn't promoted yet and the workflow times out.
    /// HashMap iteration order is unspecified, so drain → Vec → stable-sort.
    pub fn drain_into<F: FnMut(BusEvent)>(&self, mut f: F) {
        let mut events: Vec<BusEvent> = Vec::new();
        for rx in self.receivers.values() {
            while let Some(event) = rx.try_recv() {
                events.push(event);
            }
        }
        events.sort_by_key(|e| {
            let is_completion_fan_out = e.source
                == nestty_core::action_registry::COMPLETION_EVENT_SOURCE
                && (e.kind.ends_with(".completed") || e.kind.ends_with(".failed"));
            if is_completion_fan_out { 0u8 } else { 1u8 }
        });
        for event in events {
            f(event);
        }
    }
}

impl Default for TriggerSubscriptions {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nestty_core::event_bus::Event;
    use nestty_core::trigger::{AwaitClause, TimeoutPolicy, Trigger, WhenSpec};
    use serde_json::{Map, Value, json};

    fn mk_trigger(name: &str, kind: &str, action: &str) -> Trigger {
        Trigger {
            name: name.into(),
            when: WhenSpec {
                event_kind: kind.into(),
                payload_match: Map::new(),
            },
            action: action.into(),
            params: Value::Null,
            condition: None,
            r#await: None,
        }
    }

    fn mk_trigger_with_await(name: &str, kind: &str, action: &str, await_kind: &str) -> Trigger {
        Trigger {
            name: name.into(),
            when: WhenSpec {
                event_kind: kind.into(),
                payload_match: Map::new(),
            },
            action: action.into(),
            params: Value::Null,
            condition: None,
            r#await: Some(AwaitClause {
                event_kind: await_kind.into(),
                payload_match: Map::new(),
                timeout_seconds: 30,
                on_timeout: TimeoutPolicy::Abort,
            }),
        }
    }

    #[test]
    fn reconcile_creates_one_receiver_per_distinct_pattern() {
        let bus = Arc::new(CoreEventBus::new());
        let mut subs = TriggerSubscriptions::new();
        subs.reconcile(
            &bus,
            &[
                mk_trigger("a", "panel.focused", "x"),
                mk_trigger("b", "terminal.cwd_changed", "y"),
            ],
        );
        assert_eq!(subs.len(), 2);
    }

    #[test]
    fn reconcile_collapses_overlapping_patterns() {
        let bus = Arc::new(CoreEventBus::new());
        let mut subs = TriggerSubscriptions::new();
        subs.reconcile(
            &bus,
            &[
                mk_trigger("a", "*", "x"),
                mk_trigger("b", "panel.focused", "y"),
            ],
        );
        // covering_patterns collapses panel.focused into the `*` umbrella.
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn reconcile_preserves_existing_receiver_across_unrelated_reload() {
        let bus = Arc::new(CoreEventBus::new());
        let mut subs = TriggerSubscriptions::new();
        subs.reconcile(&bus, &[mk_trigger("a", "panel.focused", "x")]);
        bus.publish(Event::new("panel.focused", "test", json!({})));
        subs.reconcile(
            &bus,
            &[
                mk_trigger("a", "panel.focused", "x"),
                mk_trigger("b", "terminal.cwd_changed", "y"),
            ],
        );
        let mut drained = Vec::new();
        subs.drain_into(|e| drained.push(e.kind.clone()));
        assert_eq!(drained, vec!["panel.focused"]);
    }

    #[test]
    fn reconcile_drops_unused_receivers() {
        let bus = Arc::new(CoreEventBus::new());
        let mut subs = TriggerSubscriptions::new();
        subs.reconcile(&bus, &[mk_trigger("a", "panel.focused", "x")]);
        subs.reconcile(&bus, &[mk_trigger("b", "terminal.cwd_changed", "y")]);
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn reconcile_subscribes_await_completion_kinds() {
        let bus = Arc::new(CoreEventBus::new());
        let mut subs = TriggerSubscriptions::new();
        subs.reconcile(
            &bus,
            &[mk_trigger_with_await(
                "a",
                "user.click",
                "system.log",
                "user.reply",
            )],
        );
        // when + await + .completed + .failed → 4 patterns,
        // none of which overlap so all 4 survive covering_patterns.
        assert_eq!(subs.len(), 4);
    }

    #[test]
    fn drain_into_orders_action_completion_before_others() {
        let bus = Arc::new(CoreEventBus::new());
        let mut subs = TriggerSubscriptions::new();
        subs.reconcile(&bus, &[mk_trigger("a", "*", "x")]);
        // Publish in "wrong" order: plain event first, then a registry
        // completion. drain_into must reorder so completion fires first.
        bus.publish(Event::new("plain.event", "test", json!({})));
        bus.publish(Event::new(
            "x.completed",
            nestty_core::action_registry::COMPLETION_EVENT_SOURCE,
            json!({}),
        ));
        let mut drained = Vec::new();
        subs.drain_into(|e| drained.push((e.kind.clone(), e.source.clone())));
        assert_eq!(drained[0].0, "x.completed");
        assert_eq!(
            drained[0].1,
            nestty_core::action_registry::COMPLETION_EVENT_SOURCE
        );
    }

    #[test]
    fn drain_into_does_not_promote_plugin_completion_kind() {
        let bus = Arc::new(CoreEventBus::new());
        let mut subs = TriggerSubscriptions::new();
        subs.reconcile(&bus, &[mk_trigger("a", "*", "x")]);
        // Plugin publishes a fake `.completed` kind from a NON-registry source —
        // should stay at normal priority.
        bus.publish(Event::new(
            "plugin.work.completed",
            "plugin.todo",
            json!({}),
        ));
        bus.publish(Event::new(
            "x.completed",
            nestty_core::action_registry::COMPLETION_EVENT_SOURCE,
            json!({}),
        ));
        let mut drained = Vec::new();
        subs.drain_into(|e| drained.push(e.kind.clone()));
        assert_eq!(drained[0], "x.completed");
        assert_eq!(drained[1], "plugin.work.completed");
    }

    #[test]
    fn pump_state_drains_context_kinds_in_isolation() {
        let bus = Arc::new(CoreEventBus::new());
        let pump = PumpState::new(&bus);
        let ctx = ContextService::new();
        bus.publish(Event::new(
            "panel.focused",
            "test",
            json!({"panel_id": "p1"}),
        ));
        bus.publish(Event::new(
            "terminal.cwd_changed",
            "test",
            json!({"panel_id": "p1", "cwd": "/tmp"}),
        ));
        pump.drain_context_only(&ctx);
        let snap = ctx.snapshot();
        assert_eq!(snap.active_panel.as_deref(), Some("p1"));
        assert_eq!(
            snap.active_cwd
                .as_deref()
                .map(|p| p.to_string_lossy().into_owned()),
            Some("/tmp".into())
        );
    }
}
