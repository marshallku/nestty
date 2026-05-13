use std::cell::RefCell;
use std::rc::Rc;

use gtk4::gdk;
use gtk4::glib;
use gtk4::prelude::*;
use serde_json::json;

use nestty_core::config::NesttyConfig;
use nestty_core::protocol::Event;

use vte4::prelude::*;
use webkit6::prelude::*;

use nestty_core::plugin::LoadedPlugin;

use crate::panel::{Panel, PanelVariant};
use crate::plugin_panel::PluginPanel;
use crate::socket::{EventBus, SocketCommand, broadcast};
use crate::split::{CloseResult, TabContent};
use crate::terminal::TerminalPanel;
use crate::webview::WebViewPanel;

pub struct TabManager {
    pub notebook: gtk4::Notebook,
    tabs: Rc<RefCell<Vec<TabContent>>>,
    focused: Rc<RefCell<Option<Rc<PanelVariant>>>>,
    config: Rc<RefCell<NesttyConfig>>,
    event_bus: EventBus,
    tab_css: gtk4::CssProvider,
    /// Custom tab titles set via rename (overrides auto-titles)
    custom_titles: Rc<RefCell<std::collections::HashMap<String, String>>>,
    /// Whether the tab bar is collapsed (icon-only mode)
    tab_bar_collapsed: Rc<RefCell<bool>>,
    /// Whether the user has explicitly toggled the tab bar state
    user_toggled: Rc<RefCell<bool>>,
    /// Loaded plugins
    plugins: Rc<Vec<LoadedPlugin>>,
    /// Sender to dispatch socket commands (for plugin JS bridge)
    dispatch_tx: std::sync::mpsc::Sender<SocketCommand>,
}

impl TabManager {
    pub fn new(
        config: &NesttyConfig,
        window: &gtk4::ApplicationWindow,
        event_bus: EventBus,
        plugins: Vec<LoadedPlugin>,
        dispatch_tx: std::sync::mpsc::Sender<SocketCommand>,
    ) -> Rc<Self> {
        let notebook = gtk4::Notebook::new();
        notebook.set_scrollable(true);
        notebook.set_show_border(false);
        notebook.set_show_tabs(true);
        notebook.set_hexpand(true);
        notebook.set_vexpand(true);

        let tab_pos = match config.tabs.position.as_str() {
            "left" => gtk4::PositionType::Left,
            "right" => gtk4::PositionType::Right,
            "bottom" => gtk4::PositionType::Bottom,
            _ => gtk4::PositionType::Top,
        };
        notebook.set_tab_pos(tab_pos);

        // Tab bar CSS
        let tab_css = gtk4::CssProvider::new();
        let theme = nestty_core::theme::Theme::by_name(&config.theme.name).unwrap_or_default();
        tab_css.load_from_string(&build_tab_css(config.tabs.width, &theme));
        gtk4::style_context_add_provider_for_display(
            &gdk::Display::default().unwrap(),
            &tab_css,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 3,
        );

        let manager = Rc::new(Self {
            notebook,
            tabs: Rc::new(RefCell::new(Vec::new())),
            focused: Rc::new(RefCell::new(None)),
            config: Rc::new(RefCell::new(config.clone())),
            event_bus,
            tab_css,
            custom_titles: Rc::new(RefCell::new(std::collections::HashMap::new())),
            tab_bar_collapsed: Rc::new(RefCell::new(config.tabs.collapsed)),
            user_toggled: Rc::new(RefCell::new(false)),
            plugins: Rc::new(plugins),
            dispatch_tx,
        });

        // Apply initial collapsed state
        if config.tabs.collapsed {
            manager.notebook.add_css_class("nestty-collapsed");
        }

        // Update tab bar visibility on page remove
        let tabs_ref = manager.tabs.clone();
        let collapsed = manager.tab_bar_collapsed.clone();
        manager
            .notebook
            .connect_page_removed(move |_notebook, _, _| {
                // Keep references alive; tab bar always visible (collapsed or expanded)
                let _ = (&tabs_ref, &collapsed);
            });

        // Focus the right panel when switching tabs
        let focused = manager.focused.clone();
        let tabs_ref = manager.tabs.clone();
        manager.notebook.connect_switch_page(move |_, _, page_num| {
            let tabs = tabs_ref.borrow();
            if let Some(tab) = tabs.get(page_num as usize) {
                let mut panels = Vec::new();
                tab.root.borrow().collect_panels(&mut panels);
                // Focus first panel in this tab, or the previously focused one if it's in this tab
                let current_focused = focused.borrow().clone();
                let should_focus = current_focused
                    .filter(|f| panels.iter().any(|p| Rc::ptr_eq(p, f)))
                    .or_else(|| panels.into_iter().next());
                if let Some(panel) = should_focus {
                    panel.grab_focus();
                }
            }
        });

        // Action buttons in the tab bar
        setup_tab_actions(&manager, window);

        // Keyboard shortcuts
        setup_shortcuts(&manager, window);

        // First tab
        let mgr = manager.clone();
        let win = window.clone();
        mgr.add_tab(&win);

        manager
    }

    pub fn add_tab(self: &Rc<Self>, window: &gtk4::ApplicationWindow) {
        let _ = self.add_tab_with_cwd(window, None);
    }

    /// Add a new terminal tab whose shell is spawned with `cwd`
    /// (None = inherit from nestty). Returns `(panel, tab_index)`.
    pub fn add_tab_with_cwd(
        self: &Rc<Self>,
        window: &gtk4::ApplicationWindow,
        cwd: Option<&std::path::Path>,
    ) -> (Rc<PanelVariant>, u32) {
        self.add_tab_with_cwd_and_initial_input(window, cwd, None)
    }

    /// Pipes `initial_input` AFTER spawn_async's success callback fires —
    /// without that ordering we'd write to a PTY with no child attached.
    pub fn add_tab_with_cwd_and_initial_input(
        self: &Rc<Self>,
        window: &gtk4::ApplicationWindow,
        cwd: Option<&std::path::Path>,
        initial_input: Option<String>,
    ) -> (Rc<PanelVariant>, u32) {
        let config = self.config.borrow().clone();
        let panel = self.create_panel(&config, window, cwd, initial_input);

        let tab_content = TabContent::new(panel.clone());
        let tab_label = self.make_tab_label(&panel, &tab_content.container);

        self.notebook
            .append_page(&tab_content.container, Some(&tab_label));
        self.notebook
            .set_tab_reorderable(&tab_content.container, true);
        self.tabs.borrow_mut().push(tab_content);

        let page_num = self.notebook.n_pages() - 1;
        self.notebook.set_current_page(Some(page_num));
        *self.focused.borrow_mut() = Some(panel.clone());
        panel.grab_focus();

        broadcast(
            &self.event_bus,
            &Event::new(
                "tab.created",
                json!({
                    "panel_id": panel.id(),
                    "panel_type": panel.panel_type(),
                    "tab": page_num,
                }),
            ),
        );
        (panel, page_num)
    }

    pub fn add_webview_tab(
        self: &Rc<Self>,
        url: &str,
        _window: &gtk4::ApplicationWindow,
    ) -> String {
        let panel = self.create_webview_panel(url);
        let panel_id = panel.id().to_string();

        let tab_content = TabContent::new(panel.clone());
        let tab_label = self.make_tab_label(&panel, &tab_content.container);

        self.notebook
            .append_page(&tab_content.container, Some(&tab_label));
        self.notebook
            .set_tab_reorderable(&tab_content.container, true);
        self.tabs.borrow_mut().push(tab_content);

        let page_num = self.notebook.n_pages() - 1;
        self.notebook.set_current_page(Some(page_num));
        *self.focused.borrow_mut() = Some(panel.clone());
        panel.grab_focus();

        broadcast(
            &self.event_bus,
            &Event::new(
                "tab.created",
                json!({
                    "panel_id": panel_id,
                    "panel_type": "webview",
                    "tab": page_num,
                }),
            ),
        );

        panel_id
    }

    pub fn add_plugin_tab(
        self: &Rc<Self>,
        plugin: &LoadedPlugin,
        panel_name: &str,
    ) -> Option<String> {
        let panel = self.create_plugin_panel(plugin, panel_name)?;
        let panel_id = panel.id().to_string();

        let tab_content = TabContent::new(panel.clone());
        let tab_label = self.make_tab_label(&panel, &tab_content.container);

        self.notebook
            .append_page(&tab_content.container, Some(&tab_label));
        self.notebook
            .set_tab_reorderable(&tab_content.container, true);
        self.tabs.borrow_mut().push(tab_content);

        let page_num = self.notebook.n_pages() - 1;
        self.notebook.set_current_page(Some(page_num));
        *self.focused.borrow_mut() = Some(panel.clone());
        panel.grab_focus();

        broadcast(
            &self.event_bus,
            &Event::new(
                "tab.created",
                json!({
                    "panel_id": panel_id,
                    "panel_type": "plugin",
                    "plugin": plugin.manifest.plugin.name,
                    "tab": page_num,
                }),
            ),
        );

        Some(panel_id)
    }

    pub fn split_focused_plugin(
        self: &Rc<Self>,
        plugin: &LoadedPlugin,
        panel_name: &str,
        orientation: gtk4::Orientation,
    ) -> Option<String> {
        let focused = self.focused.borrow().clone();
        let focused_panel = focused?;
        let tab_idx = self.tab_index_of(&focused_panel)?;

        let new_panel = self.create_plugin_panel(plugin, panel_name)?;
        let panel_id = new_panel.id().to_string();

        {
            let tabs = self.tabs.borrow();
            tabs[tab_idx].split(&focused_panel, &new_panel, orientation);
        }

        *self.focused.borrow_mut() = Some(new_panel.clone());
        new_panel.grab_focus();

        Some(panel_id)
    }

    pub fn plugins(&self) -> &[LoadedPlugin] {
        &self.plugins
    }

    pub fn split_focused(
        self: &Rc<Self>,
        orientation: gtk4::Orientation,
        window: &gtk4::ApplicationWindow,
    ) {
        let focused = self.focused.borrow().clone();
        let Some(focused_panel) = focused else { return };
        let Some(tab_idx) = self.tab_index_of(&focused_panel) else {
            return;
        };

        let config = self.config.borrow().clone();
        let new_panel = self.create_panel(&config, window, None, None);

        {
            let tabs = self.tabs.borrow();
            tabs[tab_idx].split(&focused_panel, &new_panel, orientation);
        }

        *self.focused.borrow_mut() = Some(new_panel.clone());
        new_panel.grab_focus();
    }

    pub fn split_focused_webview(
        self: &Rc<Self>,
        url: &str,
        orientation: gtk4::Orientation,
        _window: &gtk4::ApplicationWindow,
    ) -> Option<String> {
        let focused = self.focused.borrow().clone();
        let focused_panel = focused?;
        let tab_idx = self.tab_index_of(&focused_panel)?;

        let new_panel = self.create_webview_panel(url);
        let panel_id = new_panel.id().to_string();

        {
            let tabs = self.tabs.borrow();
            tabs[tab_idx].split(&focused_panel, &new_panel, orientation);
        }

        *self.focused.borrow_mut() = Some(new_panel.clone());
        new_panel.grab_focus();

        Some(panel_id)
    }

    pub fn close_focused(self: &Rc<Self>, window: &gtk4::ApplicationWindow) {
        let focused = self.focused.borrow().clone();
        let Some(focused_panel) = focused else { return };
        let Some(tab_idx) = self.tab_index_of(&focused_panel) else {
            return;
        };

        let result = {
            let tabs = self.tabs.borrow();
            tabs[tab_idx].close_panel(&focused_panel)
        };

        match result {
            CloseResult::CloseTab => {
                let panel_id = focused_panel.id().to_string();
                self.tabs.borrow_mut().remove(tab_idx);
                self.notebook.remove_page(Some(tab_idx as u32));

                broadcast(
                    &self.event_bus,
                    &Event::new(
                        "tab.closed",
                        json!({
                            "panel_id": panel_id,
                            "tab": tab_idx,
                        }),
                    ),
                );

                if self.tabs.borrow().is_empty() {
                    window.close();
                    return;
                }
                self.focus_active_tab_panel();
            }
            CloseResult::Closed { focus_target } => {
                if let Some(panel) = focus_target {
                    *self.focused.borrow_mut() = Some(panel.clone());
                    panel.grab_focus();
                } else {
                    // Fallback: focus any panel in the same tab
                    let tabs = self.tabs.borrow();
                    let mut panels = Vec::new();
                    tabs[tab_idx].root.borrow().collect_panels(&mut panels);
                    if let Some(panel) = panels.first() {
                        *self.focused.borrow_mut() = Some(panel.clone());
                        panel.grab_focus();
                    }
                }
            }
        }
    }

    pub fn active_panel(&self) -> Option<Rc<PanelVariant>> {
        self.focused.borrow().clone()
    }

    // -- Tab bar toggle --

    /// Toggle tab bar between expanded and collapsed (icon-only) mode.
    /// Returns true if now expanded.
    pub fn toggle_tab_bar(&self) -> bool {
        *self.user_toggled.borrow_mut() = true;
        let collapsed = {
            let mut c = self.tab_bar_collapsed.borrow_mut();
            *c = !*c;
            *c
        };
        self.apply_collapsed_state(collapsed);
        !collapsed
    }

    fn apply_collapsed_state(&self, collapsed: bool) {
        // Toggle CSS class on notebook for width changes
        if collapsed {
            self.notebook.add_css_class("nestty-collapsed");
        } else {
            self.notebook.remove_css_class("nestty-collapsed");
        }

        // Show/hide label + close button on each tab
        let tabs = self.tabs.borrow();
        for tab in tabs.iter() {
            if let Some(tab_label) = self.notebook.tab_label(&tab.container)
                && let Some(hbox) = tab_label.downcast_ref::<gtk4::Box>()
            {
                // Children: [Icon, Label, CloseButton]
                let mut child = hbox.first_child();
                let mut idx = 0;
                while let Some(widget) = child {
                    child = widget.next_sibling();
                    if idx > 0 {
                        widget.set_visible(!collapsed);
                    }
                    idx += 1;
                }
            }
        }

        // Reorient/reorder the action widget (toggle + add) so the add
        // button stays reachable even when a vertical tab column is too
        // narrow to host both buttons side-by-side.
        self.update_action_box_layout(collapsed);

        self.notebook.set_show_tabs(true);
    }

    /// Vertical+collapsed: stack `[add, toggle]` so the `+` button isn't
    /// hidden by the narrow column. Otherwise: horizontal `[toggle, add]`.
    fn update_action_box_layout(&self, collapsed: bool) {
        let Some(action) = self.notebook.action_widget(gtk4::PackType::End) else {
            return;
        };
        let Some(hbox) = action.downcast_ref::<gtk4::Box>() else {
            return;
        };

        // Identify the two buttons by widget name (set in setup_tab_actions).
        // Walking siblings instead of indexing because reorder_child_after
        // changes positions on each call.
        let mut toggle: Option<gtk4::Widget> = None;
        let mut add: Option<gtk4::Widget> = None;
        let mut child = hbox.first_child();
        while let Some(w) = child {
            child = w.next_sibling();
            match w.widget_name().as_str() {
                "nestty-tab-toggle" => toggle = Some(w),
                "nestty-tab-add" => add = Some(w),
                _ => {}
            }
        }
        let (Some(toggle), Some(add)) = (toggle, add) else {
            return;
        };

        let stacked = self.is_vertical_tabs() && collapsed;

        hbox.set_orientation(if stacked {
            gtk4::Orientation::Vertical
        } else {
            gtk4::Orientation::Horizontal
        });
        hbox.set_halign(if stacked {
            gtk4::Align::Center
        } else {
            gtk4::Align::Start
        });

        toggle.set_visible(true);
        add.set_visible(true);

        if stacked {
            // [add, toggle] — `+` sits above the toggle button.
            hbox.reorder_child_after(&toggle, Some(&add));
        } else {
            // [toggle, add] — original horizontal layout.
            hbox.reorder_child_after(&add, Some(&toggle));
        }
    }

    // -- Tab rename --

    /// Rename a tab by panel ID. Returns true if found.
    pub fn rename_tab(&self, panel_id: &str, title: &str) -> bool {
        // Find the tab containing this panel
        let tabs = self.tabs.borrow();
        for (idx, tab) in tabs.iter().enumerate() {
            let mut panels = Vec::new();
            tab.root.borrow().collect_panels(&mut panels);
            if panels.iter().any(|p| p.id() == panel_id) {
                // Update the notebook tab label text
                if let Some(tab_label) = self.notebook.tab_label(&tab.container)
                    && let Some(icon) = tab_label.first_child()
                    && let Some(label_widget) = icon.next_sibling()
                    && let Some(label) = label_widget.downcast_ref::<gtk4::Label>()
                {
                    label.set_text(title);
                }
                // Store custom title
                self.custom_titles
                    .borrow_mut()
                    .insert(panel_id.to_string(), title.to_string());

                broadcast(
                    &self.event_bus,
                    &Event::new(
                        "tab.renamed",
                        json!({ "panel_id": panel_id, "title": title, "tab": idx }),
                    ),
                );
                return true;
            }
        }
        false
    }

    pub fn tab_count(&self) -> usize {
        self.tabs.borrow().len()
    }

    pub fn current_tab(&self) -> Option<u32> {
        self.notebook.current_page()
    }

    pub fn current_theme_name(&self) -> String {
        self.config.borrow().theme.name.clone()
    }

    pub fn update_config(&self, config: &NesttyConfig) {
        *self.config.borrow_mut() = config.clone();

        let tab_pos = match config.tabs.position.as_str() {
            "left" => gtk4::PositionType::Left,
            "right" => gtk4::PositionType::Right,
            "bottom" => gtk4::PositionType::Bottom,
            _ => gtk4::PositionType::Top,
        };
        self.notebook.set_tab_pos(tab_pos);
        let theme = nestty_core::theme::Theme::by_name(&config.theme.name).unwrap_or_default();
        self.tab_css
            .load_from_string(&build_tab_css(config.tabs.width, &theme));

        // Apply collapsed config if user hasn't manually toggled
        if !*self.user_toggled.borrow() {
            *self.tab_bar_collapsed.borrow_mut() = config.tabs.collapsed;
            self.apply_collapsed_state(config.tabs.collapsed);
        } else {
            // Position may have flipped between vertical and horizontal
            // even if collapsed state didn't change — re-lay out the
            // action widget so a manually-collapsed user keeps the add
            // button visible (stacked) on left/right tabs.
            self.update_action_box_layout(*self.tab_bar_collapsed.borrow());
        }

        for tab in self.tabs.borrow().iter() {
            let mut panels = Vec::new();
            tab.root.borrow().collect_panels(&mut panels);
            for panel in panels {
                if let Some(term) = panel.as_terminal() {
                    term.apply_config(config);
                }
            }
        }
    }

    /// Navigate focus between split panes
    pub fn focus_direction(&self, direction: FocusDirection) {
        let focused = self.focused.borrow().clone();
        let Some(focused_panel) = focused else { return };
        let Some(tab_idx) = self.tab_index_of(&focused_panel) else {
            return;
        };

        let tabs = self.tabs.borrow();
        let mut panels = Vec::new();
        tabs[tab_idx].root.borrow().collect_panels(&mut panels);

        if panels.len() < 2 {
            return;
        }

        // Simple: cycle through panels in order based on direction
        let current_idx = panels
            .iter()
            .position(|p| Rc::ptr_eq(p, &focused_panel))
            .unwrap_or(0);

        let next_idx = match direction {
            FocusDirection::Next => (current_idx + 1) % panels.len(),
            FocusDirection::Prev => {
                if current_idx == 0 {
                    panels.len() - 1
                } else {
                    current_idx - 1
                }
            }
        };

        let next_panel = &panels[next_idx];
        *self.focused.borrow_mut() = Some(next_panel.clone());
        next_panel.grab_focus();
    }

    /// Return info for all panels across all tabs
    pub fn all_panels_info(&self) -> Vec<serde_json::Value> {
        let tabs = self.tabs.borrow();
        let focused = self.focused.borrow().clone();
        let mut result = Vec::new();

        for (tab_idx, tab) in tabs.iter().enumerate() {
            let mut panels = Vec::new();
            tab.root.borrow().collect_panels(&mut panels);
            for panel in panels {
                let is_focused = focused.as_ref().is_some_and(|f| Rc::ptr_eq(f, &panel));
                let mut info = json!({
                    "id": panel.id(),
                    "type": panel.panel_type(),
                    "title": panel.title(),
                    "tab": tab_idx,
                    "focused": is_focused,
                });
                if let Some(wv) = panel.as_webview() {
                    info["url"] = json!(wv.current_url());
                }
                result.push(info);
            }
        }

        result
    }

    /// Return detailed info for a panel by ID
    pub fn panel_info_by_id(&self, id: &str) -> Option<serde_json::Value> {
        let tabs = self.tabs.borrow();
        let focused = self.focused.borrow().clone();

        for (tab_idx, tab) in tabs.iter().enumerate() {
            let mut panels = Vec::new();
            tab.root.borrow().collect_panels(&mut panels);
            for panel in panels {
                if panel.id() == id {
                    let is_focused = focused.as_ref().is_some_and(|f| Rc::ptr_eq(f, &panel));
                    let mut info = json!({
                        "id": panel.id(),
                        "type": panel.panel_type(),
                        "title": panel.title(),
                        "tab": tab_idx,
                        "focused": is_focused,
                    });
                    match &*panel {
                        PanelVariant::Terminal(term) => {
                            let (cursor_row, cursor_col) = term.terminal.cursor_position();
                            info["cols"] = json!(term.terminal.column_count());
                            info["rows"] = json!(term.terminal.row_count());
                            info["cursor"] = json!([cursor_row, cursor_col]);
                        }
                        PanelVariant::WebView(wv) => {
                            info["url"] = json!(wv.current_url());
                        }
                        PanelVariant::Plugin(pp) => {
                            info["plugin"] = json!(pp.plugin_name);
                            info["panel_name"] = json!(pp.panel_name);
                        }
                    }
                    return Some(info);
                }
            }
        }

        None
    }

    /// Find a panel by ID
    pub fn find_panel_by_id(&self, id: &str) -> Option<Rc<PanelVariant>> {
        let tabs = self.tabs.borrow();
        for tab in tabs.iter() {
            let mut panels = Vec::new();
            tab.root.borrow().collect_panels(&mut panels);
            for panel in panels {
                if panel.id() == id {
                    return Some(panel);
                }
            }
        }
        None
    }

    /// Find the first terminal panel across all tabs.
    pub fn find_first_terminal(&self) -> Option<Rc<PanelVariant>> {
        let tabs = self.tabs.borrow();
        for tab in tabs.iter() {
            let mut panels = Vec::new();
            tab.root.borrow().collect_panels(&mut panels);
            for panel in panels {
                if panel.as_terminal().is_some() {
                    return Some(panel);
                }
            }
        }
        None
    }

    /// Return extended tab info
    pub fn tab_info(&self) -> serde_json::Value {
        let tabs = self.tabs.borrow();
        let current = self.notebook.current_page();
        let mut tab_list = Vec::new();

        for (i, tab) in tabs.iter().enumerate() {
            let mut panels = Vec::new();
            tab.root.borrow().collect_panels(&mut panels);
            let title = panels.first().map(|p| p.title()).unwrap_or_default();
            tab_list.push(json!({
                "index": i,
                "panel_count": panels.len(),
                "title": title,
            }));
        }

        json!({
            "count": tabs.len(),
            "current": current,
            "tabs": tab_list,
        })
    }

    // -- Private helpers --

    fn create_panel(
        self: &Rc<Self>,
        config: &NesttyConfig,
        window: &gtk4::ApplicationWindow,
        cwd: Option<&std::path::Path>,
        initial_input: Option<String>,
    ) -> Rc<PanelVariant> {
        let mgr = Rc::downgrade(self);
        let win = window.clone();
        let widget_holder: Rc<RefCell<Option<gtk4::Widget>>> = Rc::new(RefCell::new(None));
        let widget_for_exit = widget_holder.clone();
        let event_bus_exit = self.event_bus.clone();

        let terminal_panel =
            TerminalPanel::new_with_cwd_and_initial_input(config, cwd, initial_input, move || {
                let widget = widget_for_exit.borrow().clone();
                let mgr = mgr.clone();
                let win = win.clone();
                let bus = event_bus_exit.clone();
                glib::idle_add_local_once(move || {
                    let Some(mgr) = mgr.upgrade() else { return };
                    if let Some(ref w) = widget {
                        mgr.handle_panel_exit(w, &win, &bus);
                    }
                });
            });

        let panel = Rc::new(PanelVariant::Terminal(terminal_panel));

        *widget_holder.borrow_mut() = Some(panel.widget().clone());

        // Hook terminal output events
        if let Some(term) = panel.as_terminal() {
            let bus = self.event_bus.clone();
            let panel_id = term.id.clone();
            term.terminal.connect_commit(move |_term, text, _size| {
                broadcast(
                    &bus,
                    &Event::new(
                        "terminal.output",
                        json!({
                            "panel_id": panel_id,
                            "text": text,
                        }),
                    ),
                );
            });

            // Hook title change events
            let bus = self.event_bus.clone();
            let panel_id = term.id.clone();
            term.terminal.connect_window_title_changed(move |term| {
                let title = term
                    .window_title()
                    .map(|t| t.to_string())
                    .unwrap_or_default();
                broadcast(
                    &bus,
                    &Event::new(
                        "panel.title_changed",
                        json!({
                            "panel_id": panel_id,
                            "title": title,
                        }),
                    ),
                );
            });

            // Hook CWD change events (OSC 7)
            let bus = self.event_bus.clone();
            let panel_id = term.id.clone();
            term.terminal
                .connect_current_directory_uri_changed(move |term| {
                    let cwd = term
                        .current_directory_uri()
                        .map(|u| crate::terminal::normalize_osc7_uri(u.as_str()));
                    broadcast(
                        &bus,
                        &Event::new(
                            "terminal.cwd_changed",
                            json!({
                                "panel_id": panel_id,
                                "cwd": cwd,
                            }),
                        ),
                    );
                });

            // Shell integration via termprop-changed (VTE ≥0.78)
            // VTE replaced shell-precmd/preexec signals with termprops.
            // Use detailed signal connections to subscribe to specific termprops.
            {
                let bus = self.event_bus.clone();
                let panel_id = term.id.clone();
                term.terminal.connect_closure(
                    "termprop-changed::vte.shell.precmd",
                    false,
                    gtk4::glib::closure_local!(move |_term: vte4::Terminal, _name: &str| {
                        broadcast(
                            &bus,
                            &Event::new("terminal.shell_precmd", json!({ "panel_id": panel_id })),
                        );
                    }),
                );

                let bus = self.event_bus.clone();
                let panel_id = term.id.clone();
                term.terminal.connect_closure(
                    "termprop-changed::vte.shell.preexec",
                    false,
                    gtk4::glib::closure_local!(move |_term: vte4::Terminal, _name: &str| {
                        broadcast(
                            &bus,
                            &Event::new("terminal.shell_preexec", json!({ "panel_id": panel_id })),
                        );
                    }),
                );
            }
        }

        self.track_focus(&panel);
        panel
    }

    fn create_webview_panel(self: &Rc<Self>, url: &str) -> Rc<PanelVariant> {
        let config = self.config.borrow();
        let theme = nestty_core::theme::Theme::by_name(&config.theme.name).unwrap_or_default();
        drop(config);
        let webview_panel = WebViewPanel::new(url, &theme);
        let panel = Rc::new(PanelVariant::WebView(webview_panel));

        // Hook webview events
        if let Some(wv) = panel.as_webview() {
            let bus = self.event_bus.clone();
            let panel_id = wv.id.clone();
            wv.webview.connect_load_changed(move |_wv, event| {
                if event == webkit6::LoadEvent::Finished {
                    broadcast(
                        &bus,
                        &Event::new(
                            "webview.loaded",
                            json!({
                                "panel_id": panel_id,
                            }),
                        ),
                    );
                }
            });

            let bus = self.event_bus.clone();
            let panel_id = wv.id.clone();
            wv.webview
                .connect_notify_local(Some("title"), move |webview, _| {
                    let title = webview.title().map(|t| t.to_string()).unwrap_or_default();
                    broadcast(
                        &bus,
                        &Event::new(
                            "webview.title_changed",
                            json!({
                                "panel_id": panel_id,
                                "title": title,
                            }),
                        ),
                    );
                });

            let bus = self.event_bus.clone();
            let panel_id = wv.id.clone();
            wv.webview
                .connect_notify_local(Some("uri"), move |webview, _| {
                    let url = webview.uri().map(|u| u.to_string()).unwrap_or_default();
                    broadcast(
                        &bus,
                        &Event::new(
                            "webview.navigated",
                            json!({
                                "panel_id": panel_id,
                                "url": url,
                            }),
                        ),
                    );
                });
        }

        self.track_focus(&panel);
        panel
    }

    fn create_plugin_panel(
        self: &Rc<Self>,
        plugin: &LoadedPlugin,
        panel_name: &str,
    ) -> Option<Rc<PanelVariant>> {
        let panel_def = plugin
            .manifest
            .panels
            .iter()
            .find(|p| p.name == panel_name)?;

        let config = self.config.borrow();
        let theme = nestty_core::theme::Theme::by_name(&config.theme.name).unwrap_or_default();
        drop(config);

        let plugin_panel = PluginPanel::new(
            plugin,
            panel_def,
            &theme,
            self.dispatch_tx.clone(),
            self.event_bus.clone(),
        );
        let panel = Rc::new(PanelVariant::Plugin(plugin_panel));
        self.track_focus(&panel);
        Some(panel)
    }

    fn track_focus(&self, panel: &Rc<PanelVariant>) {
        let focused = self.focused.clone();
        let panel_weak = Rc::downgrade(panel);
        let bus = self.event_bus.clone();
        let controller = gtk4::EventControllerFocus::new();
        controller.connect_enter(move |_| {
            if let Some(panel) = panel_weak.upgrade() {
                let panel_id = panel.id().to_string();
                *focused.borrow_mut() = Some(panel);
                broadcast(
                    &bus,
                    &Event::new(
                        "panel.focused",
                        json!({
                            "panel_id": panel_id,
                        }),
                    ),
                );
            }
        });

        // Attach focus controller to the inner focusable widget
        match &**panel {
            PanelVariant::Terminal(term) => {
                term.terminal.add_controller(controller);
            }
            PanelVariant::WebView(wv) => {
                wv.webview.add_controller(controller);
            }
            PanelVariant::Plugin(pp) => {
                pp.webview.add_controller(controller);
            }
        }
    }

    fn handle_panel_exit(
        &self,
        panel_widget: &gtk4::Widget,
        window: &gtk4::ApplicationWindow,
        bus: &EventBus,
    ) {
        let tabs = self.tabs.borrow();
        for (tab_idx, tab) in tabs.iter().enumerate() {
            let mut panels = Vec::new();
            tab.root.borrow().collect_panels(&mut panels);
            if let Some(panel) = panels.iter().find(|p| p.widget() == panel_widget) {
                let panel_id = panel.id().to_string();
                let result = tab.close_panel(panel);

                broadcast(
                    bus,
                    &Event::new(
                        "panel.exited",
                        json!({
                            "panel_id": panel_id,
                            "tab": tab_idx,
                        }),
                    ),
                );

                match result {
                    CloseResult::CloseTab => {
                        drop(tabs);
                        self.tabs.borrow_mut().remove(tab_idx);
                        self.notebook.remove_page(Some(tab_idx as u32));

                        broadcast(
                            bus,
                            &Event::new(
                                "tab.closed",
                                json!({
                                    "panel_id": panel_id,
                                    "tab": tab_idx,
                                }),
                            ),
                        );

                        if self.tabs.borrow().is_empty() {
                            window.close();
                            return;
                        }
                        self.focus_active_tab_panel();
                    }
                    CloseResult::Closed { focus_target } => {
                        if let Some(p) = focus_target {
                            *self.focused.borrow_mut() = Some(p.clone());
                            p.grab_focus();
                        } else {
                            let mut remaining = Vec::new();
                            tab.root.borrow().collect_panels(&mut remaining);
                            if let Some(p) = remaining.first() {
                                *self.focused.borrow_mut() = Some(p.clone());
                                p.grab_focus();
                            }
                        }
                    }
                }
                return;
            }
        }
    }

    fn tab_index_of(&self, panel: &Rc<PanelVariant>) -> Option<usize> {
        let tabs = self.tabs.borrow();
        for (i, tab) in tabs.iter().enumerate() {
            let mut panels = Vec::new();
            tab.root.borrow().collect_panels(&mut panels);
            if panels.iter().any(|p| Rc::ptr_eq(p, panel)) {
                return Some(i);
            }
        }
        None
    }

    fn focus_active_tab_panel(&self) {
        if let Some(page) = self.notebook.current_page() {
            let tabs = self.tabs.borrow();
            if let Some(tab) = tabs.get(page as usize) {
                let mut panels = Vec::new();
                tab.root.borrow().collect_panels(&mut panels);
                if let Some(p) = panels.first() {
                    *self.focused.borrow_mut() = Some(p.clone());
                    p.grab_focus();
                }
            }
        }
    }

    fn is_vertical_tabs(&self) -> bool {
        matches!(
            self.notebook.tab_pos(),
            gtk4::PositionType::Left | gtk4::PositionType::Right
        )
    }

    fn make_tab_label(&self, panel: &Rc<PanelVariant>, page_container: &gtk4::Box) -> gtk4::Box {
        let hbox = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
        let vertical = self.is_vertical_tabs();
        let (icon_name, default_title) = match &**panel {
            PanelVariant::Terminal(_) => ("utilities-terminal-symbolic".to_string(), "Terminal"),
            PanelVariant::WebView(_) => ("web-browser-symbolic".to_string(), "WebView"),
            PanelVariant::Plugin(pp) => {
                // `resolve_by_name` honors the same duplicate-winner rule
                // as daemon dispatch and `plugin.open`, so the tab icon
                // matches the manifest those paths actually run.
                let icon_name =
                    nestty_core::plugin::resolve_by_name(&self.plugins, &pp.plugin_name)
                        .and_then(|p| p.manifest.panels.iter().find(|pd| pd.name == pp.panel_name))
                        .and_then(|pd| pd.icon.clone())
                        .unwrap_or_else(|| "application-x-addon-symbolic".to_string());
                (icon_name, "Plugin")
            }
        };

        let icon = gtk4::Image::from_icon_name(&icon_name);
        icon.add_css_class("nestty-tab-icon");

        let label = gtk4::Label::new(Some(default_title));
        label.add_css_class("nestty-tab-label");
        label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        if vertical {
            label.set_hexpand(true);
            label.set_xalign(0.0);
            label.set_max_width_chars(16);
        } else {
            label.set_hexpand(true);
            label.set_max_width_chars(20);
        }

        let close_btn = gtk4::Button::from_icon_name("window-close-symbolic");
        close_btn.add_css_class("flat");
        close_btn.add_css_class("nestty-tab-close");
        close_btn.set_tooltip_text(Some("Close tab"));

        // Order: [Icon, Label, CloseButton]
        hbox.append(&icon);
        hbox.append(&label);
        hbox.append(&close_btn);

        // If currently collapsed, hide label and close button
        if *self.tab_bar_collapsed.borrow() {
            label.set_visible(false);
            close_btn.set_visible(false);
        }

        // Hook title updates based on panel type (suppressed when custom title is set)
        let panel_id_for_title = panel.id().to_string();
        match &**panel {
            PanelVariant::Terminal(term) => {
                let label_clone = label.clone();
                let custom = self.custom_titles.clone();
                let pid = panel_id_for_title.clone();
                term.terminal
                    .connect_window_title_changed(move |term: &vte4::Terminal| {
                        if custom.borrow().contains_key(&pid) {
                            return;
                        }
                        if let Some(title) = term.window_title() {
                            label_clone.set_text(&title);
                        }
                    });
            }
            PanelVariant::WebView(wv) => {
                let label_clone = label.clone();
                let custom = self.custom_titles.clone();
                let pid = panel_id_for_title.clone();
                wv.webview
                    .connect_notify_local(Some("title"), move |webview, _| {
                        if custom.borrow().contains_key(&pid) {
                            return;
                        }
                        if let Some(title) = webview.title() {
                            label_clone.set_text(&title);
                        }
                    });
            }
            PanelVariant::Plugin(_) => {
                // Plugin panels have a static title set at creation
            }
        }

        // Double-click to rename tab
        {
            let gesture = gtk4::GestureClick::new();
            gesture.set_button(1);
            let label_clone = label.clone();
            let custom = self.custom_titles.clone();
            let bus = self.event_bus.clone();
            let pid = panel_id_for_title;
            gesture.connect_released(move |gesture, n_press, _x, _y| {
                if n_press != 2 {
                    return;
                }
                gesture.set_state(gtk4::EventSequenceState::Claimed);

                // Replace label with an entry for inline editing
                let parent = label_clone.parent().unwrap();
                let hbox = parent.downcast_ref::<gtk4::Box>().unwrap();
                let current_text = label_clone.text().to_string();

                let entry = gtk4::Entry::new();
                entry.set_text(&current_text);
                entry.set_hexpand(true);

                label_clone.set_visible(false);
                hbox.prepend(&entry);
                entry.grab_focus();
                entry.select_region(0, -1);

                let label_for_activate = label_clone.clone();
                let custom_for_activate = custom.clone();
                let bus_for_activate = bus.clone();
                let pid_for_activate = pid.clone();
                let entry_clone = entry.clone();
                entry.connect_activate(move |entry| {
                    let new_title = entry.text().to_string();
                    if !new_title.is_empty() {
                        label_for_activate.set_text(&new_title);
                        custom_for_activate
                            .borrow_mut()
                            .insert(pid_for_activate.clone(), new_title.clone());
                        broadcast(
                            &bus_for_activate,
                            &Event::new(
                                "tab.renamed",
                                json!({ "panel_id": pid_for_activate, "title": new_title }),
                            ),
                        );
                    }
                    label_for_activate.set_visible(true);
                    if let Some(parent) = entry_clone.parent()
                        && let Some(hbox) = parent.downcast_ref::<gtk4::Box>()
                    {
                        hbox.remove(&entry_clone);
                    }
                });

                // Also handle focus-out (cancel/accept)
                let label_for_focus = label_clone.clone();
                let focus_ctrl = gtk4::EventControllerFocus::new();
                let entry_for_focus = entry.clone();
                focus_ctrl.connect_leave(move |_| {
                    label_for_focus.set_visible(true);
                    if let Some(parent) = entry_for_focus.parent()
                        && let Some(hbox) = parent.downcast_ref::<gtk4::Box>()
                    {
                        hbox.remove(&entry_for_focus);
                    }
                });
                entry.add_controller(focus_ctrl);
            });
            hbox.add_controller(gesture);
        }

        let nb = self.notebook.clone();
        let tabs = self.tabs.clone();
        let focused = self.focused.clone();
        let container = page_container.clone();
        let bus = self.event_bus.clone();
        close_btn.connect_clicked(move |_| {
            let Some(idx) = nb.page_num(&container) else {
                eprintln!("[nestty] close: page not found");
                return;
            };
            let idx = idx as usize;
            eprintln!("[nestty] close: removing tab {idx}");

            // Collect panel id before removing
            let panel_id = {
                let tabs_ref = tabs.borrow();
                if let Some(tab) = tabs_ref.get(idx) {
                    let mut panels = Vec::new();
                    tab.root.borrow().collect_panels(&mut panels);
                    panels.first().map(|p| p.id().to_string())
                } else {
                    None
                }
            };

            tabs.borrow_mut().remove(idx);
            nb.remove_page(Some(idx as u32));

            broadcast(
                &bus,
                &Event::new(
                    "tab.closed",
                    json!({
                        "panel_id": panel_id.as_deref().unwrap_or(""),
                        "tab": idx,
                    }),
                ),
            );

            // Handle last-tab-close: spawn new default tab is not possible here
            // (no window ref), so close the window via the notebook's toplevel
            if tabs.borrow().is_empty() {
                if let Some(root) = nb.root()
                    && let Some(window) = root.downcast_ref::<gtk4::Window>()
                {
                    window.close();
                }
                return;
            }

            // Update focus
            if let Some(new_page) = nb.current_page() {
                let tabs_ref = tabs.borrow();
                if let Some(tab) = tabs_ref.get(new_page as usize) {
                    let mut panels = Vec::new();
                    tab.root.borrow().collect_panels(&mut panels);
                    if let Some(p) = panels.first() {
                        *focused.borrow_mut() = Some(p.clone());
                        p.grab_focus();
                    }
                }
            }
        });

        hbox
    }
}

#[derive(Debug, Clone, Copy)]
pub enum FocusDirection {
    Next,
    Prev,
}

fn spawn_command(command: &str) {
    let cmd = command.strip_prefix("spawn:").unwrap_or(command);

    let expanded = shellexpand::tilde(cmd).to_string();
    let socket_path = nestty_core::paths::gui_socket_path(std::process::id())
        .to_string_lossy()
        .into_owned();

    std::thread::spawn(move || {
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(&expanded)
            .env("NESTTY_SOCKET", &socket_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    });
}

fn check_custom_keybinding(
    mgr: &TabManager,
    keyval: gdk::Key,
    keycode: u32,
    modifier: gdk::ModifierType,
) -> bool {
    let ctrl = modifier.contains(gdk::ModifierType::CONTROL_MASK);
    let shift = modifier.contains(gdk::ModifierType::SHIFT_MASK);
    let alt = modifier.contains(gdk::ModifierType::ALT_MASK);

    let config = mgr.config.borrow();
    let bindings = config.keybindings.parse();

    let key_name = keyval.name().map(|n| n.to_string().to_lowercase());
    let Some(key_name) = key_name else {
        return false;
    };

    // When shift is held, GDK gives us the shifted keyval (e.g. braceright instead of bracketright).
    // Also resolve the unshifted key from the hardware keycode for matching.
    let unshifted_name = if shift {
        gdk::Display::default().and_then(|d| {
            let entries = d.map_keycode(keycode);
            entries
                .iter()
                .flatten()
                .find(|(k, _)| k.group() == 0 && k.level() == 0)
                .and_then(|(_, v)| v.name().map(|n| n.to_string().to_lowercase()))
        })
    } else {
        None
    };

    for binding in &bindings {
        if binding.ctrl != ctrl || binding.shift != shift || binding.alt != alt {
            continue;
        }
        if binding.key == key_name {
            spawn_command(&binding.command);
            return true;
        }
        if let Some(ref unshifted) = unshifted_name
            && binding.key == *unshifted
        {
            spawn_command(&binding.command);
            return true;
        }
    }

    false
}

fn setup_shortcuts(manager: &Rc<TabManager>, window: &gtk4::ApplicationWindow) {
    let controller = gtk4::EventControllerKey::new();
    let mgr = Rc::downgrade(manager);
    let win = window.clone();

    controller.set_propagation_phase(gtk4::PropagationPhase::Capture);
    controller.connect_key_pressed(move |_, keyval, keycode, modifier| {
        let Some(mgr) = mgr.upgrade() else {
            return glib::Propagation::Proceed;
        };

        // Check custom keybindings first (from config)
        if check_custom_keybinding(&mgr, keyval, keycode, modifier) {
            return glib::Propagation::Stop;
        }

        let ctrl = modifier.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = modifier.contains(gdk::ModifierType::SHIFT_MASK);
        let ctrl_shift = ctrl && shift;

        let panel = mgr.active_panel();
        let is_terminal = panel.as_ref().is_some_and(|p| p.as_terminal().is_some());

        // Only intercept Ctrl+Shift — all Ctrl-only keys pass through to terminal/webview
        if !ctrl_shift {
            return glib::Propagation::Proceed;
        }

        match keyval {
            // Ctrl+Shift+B: toggle tab bar visibility
            gdk::Key::B => {
                mgr.toggle_tab_bar();
                glib::Propagation::Stop
            }
            // Ctrl+Shift+F: toggle search (terminal only)
            gdk::Key::F if is_terminal => {
                if let Some(term) = panel.as_ref().and_then(|p| p.as_terminal()) {
                    term.search_bar.toggle(&term.terminal);
                }
                glib::Propagation::Stop
            }
            // Ctrl+Shift+C: copy (terminal)
            gdk::Key::C if is_terminal => {
                if let Some(term) = panel.as_ref().and_then(|p| p.as_terminal()) {
                    term.terminal.copy_clipboard_format(vte4::Format::Text);
                }
                glib::Propagation::Stop
            }
            // Ctrl+Shift+V: paste (terminal)
            gdk::Key::V if is_terminal => {
                if let Some(term) = panel.as_ref().and_then(|p| p.as_terminal()) {
                    term.terminal.paste_clipboard();
                }
                glib::Propagation::Stop
            }
            // Ctrl+Shift+T: new tab
            gdk::Key::T => {
                mgr.add_tab(&win);
                glib::Propagation::Stop
            }
            // Ctrl+Shift+W: close focused panel (unsplit or close tab)
            gdk::Key::W => {
                mgr.close_focused(&win);
                glib::Propagation::Stop
            }
            // Ctrl+Shift+E: split horizontal
            gdk::Key::E => {
                mgr.split_focused(gtk4::Orientation::Horizontal, &win);
                glib::Propagation::Stop
            }
            // Ctrl+Shift+O: split vertical
            gdk::Key::O => {
                mgr.split_focused(gtk4::Orientation::Vertical, &win);
                glib::Propagation::Stop
            }
            // Ctrl+Shift+N / Ctrl+Shift+Right: next pane
            gdk::Key::N | gdk::Key::Right => {
                mgr.focus_direction(FocusDirection::Next);
                glib::Propagation::Stop
            }
            // Ctrl+Shift+P / Ctrl+Shift+Left: prev pane
            gdk::Key::P | gdk::Key::Left => {
                mgr.focus_direction(FocusDirection::Prev);
                glib::Propagation::Stop
            }
            // Ctrl+Shift+Tab: next tab
            gdk::Key::ISO_Left_Tab => {
                let nb = &mgr.notebook;
                if nb.n_pages() > 1 {
                    let current = nb.current_page().unwrap_or(0);
                    let next = (current + 1) % nb.n_pages();
                    nb.set_current_page(Some(next));
                }
                glib::Propagation::Stop
            }
            // Ctrl+Shift+1-9: switch to tab N
            k @ (gdk::Key::exclam
            | gdk::Key::at
            | gdk::Key::numbersign
            | gdk::Key::dollar
            | gdk::Key::percent
            | gdk::Key::asciicircum
            | gdk::Key::ampersand
            | gdk::Key::asterisk
            | gdk::Key::parenleft) => {
                let tab_num = match k {
                    gdk::Key::exclam => 0,
                    gdk::Key::at => 1,
                    gdk::Key::numbersign => 2,
                    gdk::Key::dollar => 3,
                    gdk::Key::percent => 4,
                    gdk::Key::asciicircum => 5,
                    gdk::Key::ampersand => 6,
                    gdk::Key::asterisk => 7,
                    gdk::Key::parenleft => 8,
                    _ => return glib::Propagation::Proceed,
                };
                if (tab_num as u32) < mgr.notebook.n_pages() {
                    mgr.notebook.set_current_page(Some(tab_num as u32));
                }
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });

    window.add_controller(controller);
}

fn setup_tab_actions(manager: &Rc<TabManager>, window: &gtk4::ApplicationWindow) {
    let action_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    action_box.add_css_class("nestty-tab-actions");
    action_box.set_halign(gtk4::Align::Start);

    // Toggle button (collapse/expand tab bar)
    let toggle_btn = gtk4::Button::from_icon_name("sidebar-show-symbolic");
    toggle_btn.add_css_class("flat");
    toggle_btn.add_css_class("nestty-action-btn");
    toggle_btn.set_tooltip_text(Some("Toggle tab bar (Ctrl+Shift+B)"));
    // Names let `update_action_box_layout` identify the two buttons
    // back from the action widget after GTK adopts the box.
    toggle_btn.set_widget_name("nestty-tab-toggle");

    let mgr = manager.clone();
    toggle_btn.connect_clicked(move |_| {
        mgr.toggle_tab_bar();
    });

    // Add button with popover for terminal/webview choice
    let add_btn = gtk4::MenuButton::new();
    add_btn.set_icon_name("list-add-symbolic");
    add_btn.add_css_class("flat");
    add_btn.add_css_class("nestty-action-btn");
    add_btn.set_tooltip_text(Some("New tab"));
    add_btn.set_widget_name("nestty-tab-add");

    let popover = gtk4::Popover::new();
    let pop_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
    pop_box.add_css_class("nestty-add-menu");

    // Helper: create a row with [TypeIcon TypeLabel] [Tab] [SplitH] [SplitV]
    let make_row =
        |icon: &str, label_text: &str| -> (gtk4::Box, gtk4::Button, gtk4::Button, gtk4::Button) {
            let row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            row.add_css_class("nestty-add-row");

            let type_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 4);
            type_box.append(&gtk4::Image::from_icon_name(icon));
            type_box.append(&gtk4::Label::new(Some(label_text)));
            type_box.set_hexpand(true);

            let tab_btn = gtk4::Button::from_icon_name("tab-new-symbolic");
            tab_btn.add_css_class("flat");
            tab_btn.add_css_class("nestty-placement-btn");
            tab_btn.set_tooltip_text(Some("New tab"));

            let split_h_btn = gtk4::Button::from_icon_name("view-dual-symbolic");
            split_h_btn.add_css_class("flat");
            split_h_btn.add_css_class("nestty-placement-btn");
            split_h_btn.set_tooltip_text(Some("Split horizontal"));

            let split_v_btn = gtk4::Button::from_icon_name("view-paged-symbolic");
            split_v_btn.add_css_class("flat");
            split_v_btn.add_css_class("nestty-placement-btn");
            split_v_btn.set_tooltip_text(Some("Split vertical"));

            row.append(&type_box);
            row.append(&tab_btn);
            row.append(&split_h_btn);
            row.append(&split_v_btn);

            (row, tab_btn, split_h_btn, split_v_btn)
        };

    let (term_row, term_tab, term_h, term_v) = make_row("utilities-terminal-symbolic", "Terminal");
    let (browser_row, browser_tab, browser_h, browser_v) =
        make_row("web-browser-symbolic", "Browser");

    pop_box.append(&term_row);
    pop_box.append(&browser_row);

    // Plugin entries — keep only the winner per duplicate name so the
    // picker matches what daemon dispatch / `plugin.open` would resolve.
    let mut seen_names = std::collections::HashSet::new();
    let plugin_winners: Vec<_> = manager
        .plugins
        .iter()
        .rev()
        .filter(|p| seen_names.insert(p.manifest.plugin.name.clone()))
        .collect();
    for plugin in plugin_winners.into_iter().rev() {
        for panel_def in &plugin.manifest.panels {
            let icon_name = panel_def
                .icon
                .as_deref()
                .unwrap_or("application-x-addon-symbolic");

            let (plugin_row, plugin_tab, plugin_h, plugin_v) =
                make_row(icon_name, &panel_def.title);
            pop_box.append(&plugin_row);

            let mgr = manager.clone();
            let pop = popover.clone();
            let p = plugin.clone();
            let pname = panel_def.name.clone();
            plugin_tab.connect_clicked(move |_| {
                pop.popdown();
                mgr.add_plugin_tab(&p, &pname);
            });

            let mgr = manager.clone();
            let pop = popover.clone();
            let p = plugin.clone();
            let pname = panel_def.name.clone();
            plugin_h.connect_clicked(move |_| {
                pop.popdown();
                mgr.split_focused_plugin(&p, &pname, gtk4::Orientation::Horizontal);
            });

            let mgr = manager.clone();
            let pop = popover.clone();
            let p = plugin.clone();
            let pname = panel_def.name.clone();
            plugin_v.connect_clicked(move |_| {
                pop.popdown();
                mgr.split_focused_plugin(&p, &pname, gtk4::Orientation::Vertical);
            });
        }
    }

    popover.set_child(Some(&pop_box));
    add_btn.set_popover(Some(&popover));

    // Terminal placements
    let mgr = manager.clone();
    let win = window.clone();
    let pop = popover.clone();
    term_tab.connect_clicked(move |_| {
        pop.popdown();
        mgr.add_tab(&win);
    });

    let mgr = manager.clone();
    let win = window.clone();
    let pop = popover.clone();
    term_h.connect_clicked(move |_| {
        pop.popdown();
        mgr.split_focused(gtk4::Orientation::Horizontal, &win);
    });

    let mgr = manager.clone();
    let win = window.clone();
    let pop = popover.clone();
    term_v.connect_clicked(move |_| {
        pop.popdown();
        mgr.split_focused(gtk4::Orientation::Vertical, &win);
    });

    // Browser placements
    let mgr = manager.clone();
    let win = window.clone();
    let pop = popover.clone();
    browser_tab.connect_clicked(move |_| {
        pop.popdown();
        mgr.add_webview_tab("about:blank", &win);
    });

    let mgr = manager.clone();
    let win = window.clone();
    let pop = popover.clone();
    browser_h.connect_clicked(move |_| {
        pop.popdown();
        mgr.split_focused_webview("about:blank", gtk4::Orientation::Horizontal, &win);
    });

    let mgr = manager.clone();
    let win = window.clone();
    let pop = popover.clone();
    browser_v.connect_clicked(move |_| {
        pop.popdown();
        mgr.split_focused_webview("about:blank", gtk4::Orientation::Vertical, &win);
    });

    action_box.append(&toggle_btn);
    action_box.append(&add_btn);

    manager
        .notebook
        .set_action_widget(&action_box, gtk4::PackType::End);

    // Apply orientation/order for the current (vertical, collapsed) combo.
    // For vertical+collapsed the column is too narrow to fit both buttons
    // side-by-side, so we stack them with the add button on top of the
    // toggle button. Keeps both reachable in collapsed mode.
    manager.update_action_box_layout(*manager.tab_bar_collapsed.borrow());
}

fn build_tab_css(tab_width: u32, theme: &nestty_core::theme::Theme) -> String {
    let bg = &theme.background;
    let surface1 = &theme.surface1;
    let surface2 = &theme.surface2;
    let overlay0 = &theme.overlay0;
    let text = &theme.text;
    let subtext0 = &theme.subtext0;
    let subtext1 = &theme.subtext1;
    let red = &theme.red;
    format!(
        r#"
notebook {{
    background-color: transparent;
}}

notebook > stack {{
    background-color: transparent;
}}

notebook header {{
    background-color: transparent;
    padding: 0;
}}

notebook header tabs {{
    background-color: transparent;
}}

notebook header tab {{
    background-color: {bg};
    color: {subtext0};
    padding: 6px 8px;
    margin: 2px 1px 0;
    border-radius: 6px 6px 0 0;
    min-height: 28px;
}}

notebook header tab:checked {{
    background-color: {surface2};
    color: {text};
}}

notebook header tab:hover:not(:checked) {{
    background-color: {surface1};
    color: {subtext1};
}}

/* Vertical tabs (left) */
notebook header.left tab {{
    border-radius: 6px 0 0 6px;
    margin: 1px 0 1px 2px;
    padding: 6px 8px;
    min-width: {tab_width}px;
    min-height: 28px;
}}

/* Vertical tabs (right) */
notebook header.right tab {{
    border-radius: 0 6px 6px 0;
    margin: 1px 2px 1px 0;
    padding: 6px 8px;
    min-width: {tab_width}px;
    min-height: 28px;
}}

/* Bottom tabs */
notebook header.bottom tab {{
    border-radius: 0 0 6px 6px;
    margin: 0 1px 2px;
    min-height: 28px;
}}

/* Collapsed mode — keep tab height, shrink width */
notebook.nestty-collapsed header.left tab,
notebook.nestty-collapsed header.right tab {{
    min-width: 0;
    padding: 6px 8px;
    min-height: 28px;
}}

notebook.nestty-collapsed header.top tab,
notebook.nestty-collapsed header.bottom tab {{
    padding: 6px 8px;
    min-height: 28px;
}}

.nestty-tab-icon {{
    min-width: 16px;
    min-height: 16px;
}}

.nestty-tab-close {{
    min-width: 16px;
    min-height: 16px;
    padding: 0;
    margin: 0;
    border-radius: 4px;
    color: {subtext0};
}}

.nestty-tab-close:hover {{
    background-color: {overlay0};
    color: {red};
}}

.nestty-tab-actions {{
    padding: 4px 6px;
    margin: 0;
}}

.nestty-action-btn,
.nestty-action-btn > button {{
    min-width: 22px;
    max-width: 22px;
    min-height: 22px;
    max-height: 22px;
    padding: 0;
    margin: 0;
    border-radius: 4px;
    color: {subtext0};
}}

.nestty-action-btn:hover,
.nestty-action-btn > button:hover {{
    background-color: {surface2};
    color: {text};
}}

.nestty-add-menu {{
    padding: 6px;
}}

.nestty-add-row {{
    padding: 4px 6px;
    border-radius: 4px;
    color: {text};
}}

.nestty-add-row:hover {{
    background-color: {surface1};
}}

.nestty-placement-btn {{
    min-width: 24px;
    min-height: 24px;
    padding: 2px;
    border-radius: 4px;
    color: {subtext0};
    opacity: 0;
}}

.nestty-add-row:hover .nestty-placement-btn {{
    opacity: 1;
}}

.nestty-placement-btn:hover {{
    background-color: {surface2};
    color: {text};
}}
"#
    )
}
