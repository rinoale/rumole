use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, TableState, Wrap},
};
use rustui::keymap::{Key, Keymap, binding};
use rustui::style::Role;

use crate::config::{Config, expand_tilde, ssh_config_hosts, tunnels_path};
use crate::history::History;
use crate::manager::{Event as ManagerEvent, EventKind, Manager};
use crate::relay;
use crate::tui::{
    command::{self, Command, Target, ThemeCommand},
    menu,
    theme::{Theme, ThemeKind},
};
use crate::tunnel::{Endpoint, TunnelKind, TunnelSpec, TunnelState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    EnterCommandMode,
    Cancel,
    Help,
    NextPane,
    PreviousPane,
    Activate,
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
    Search,
    Connect,
    Disconnect,
    Reconnect,
    Add,
    Edit,
    Delete,
    History,
    ConnectAll,
    DisconnectAll,
}

fn default_keymap() -> Keymap<Intent> {
    let none = KeyModifiers::NONE;
    Keymap::new(vec![
        binding(Key::Char('c'), none, Intent::Connect, "c", "connect"),
        binding(Key::Char('d'), none, Intent::Disconnect, "d", "disconnect"),
        binding(Key::Char('r'), none, Intent::Reconnect, "r", "reconnect"),
        binding(Key::Char('a'), none, Intent::Add, "a", "add"),
        binding(Key::Char('e'), none, Intent::Edit, "e", "edit"),
        binding(Key::Char('x'), none, Intent::Delete, "x", "delete"),
        binding(Key::Char('h'), none, Intent::History, "h", "history"),
        binding(Key::Char('C'), none, Intent::ConnectAll, "C", "connect all"),
        binding(Key::Char('D'), none, Intent::DisconnectAll, "D", "disconnect all"),
        binding(Key::Enter, none, Intent::Activate, "Enter", "toggle connect"),
        binding(Key::Char(':'), none, Intent::EnterCommandMode, ":", "command mode"),
        binding(Key::Char('?'), none, Intent::Help, "?", "help"),
        binding(Key::Char('/'), none, Intent::Search, "/", "filter"),
        binding(Key::Esc, none, Intent::Cancel, "Esc", "cancel"),
        binding(Key::Tab, none, Intent::NextPane, "Tab", "next pane"),
        binding(Key::BackTab, none, Intent::PreviousPane, "Shift-Tab", "previous pane"),
        binding(Key::Up, none, Intent::MoveUp, "Up", "move up"),
        binding(Key::Down, none, Intent::MoveDown, "Down", "move down"),
        binding(Key::Char('k'), none, Intent::MoveUp, "k", "move up"),
        binding(Key::Char('j'), none, Intent::MoveDown, "j", "move down"),
        binding(Key::PageUp, none, Intent::PageUp, "PgUp", "page up"),
        binding(Key::PageDown, none, Intent::PageDown, "PgDn", "page down"),
        binding(Key::Home, none, Intent::Top, "Home", "first tunnel"),
        binding(Key::End, none, Intent::Bottom, "End", "last tunnel"),
    ])
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Table,
    Detail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusKind {
    Info,
    Success,
    Warning,
    Error,
}

const RATE_HISTORY: usize = 120;
const LOG_LINES: usize = 200;

#[derive(Default)]
struct TunnelUi {
    state: TunnelState,
    counters: Option<relay::SharedCounters>,
    /// Totals carried over from previous sessions of this tunnel.
    accum_up: u64,
    accum_down: u64,
    /// Last sampled totals of the current session (oriented).
    prev_up: u64,
    prev_down: u64,
    up_rate: u64,
    down_rate: u64,
    up_hist: VecDeque<u64>,
    down_hist: VecDeque<u64>,
    last_error: Option<String>,
    log: VecDeque<String>,
}

impl TunnelUi {
    /// Session totals oriented as (toward remote, toward local).
    fn session_totals(&self, kind: TunnelKind) -> (u64, u64) {
        let Some(counters) = &self.counters else {
            return (0, 0);
        };
        let a2b = counters.client_to_upstream.load(Ordering::Relaxed);
        let b2a = counters.upstream_to_client.load(Ordering::Relaxed);
        match kind {
            // For -R the relay's "clients" are remote-originated connections
            // delivered by ssh, so the directions flip.
            TunnelKind::Remote => (b2a, a2b),
            _ => (a2b, b2a),
        }
    }

    fn totals(&self, kind: TunnelKind) -> (u64, u64) {
        let (up, down) = self.session_totals(kind);
        (self.accum_up + up, self.accum_down + down)
    }

    fn connections(&self) -> Option<(u64, u64)> {
        self.counters.as_ref().map(|c| {
            (
                c.active_connections.load(Ordering::Relaxed),
                c.total_connections.load(Ordering::Relaxed),
            )
        })
    }
}

#[derive(Debug, Clone)]
struct Form {
    editing: Option<String>,
    field: usize,
    name: String,
    host: String,
    kind: TunnelKind,
    listen: String,
    target: String,
    auto_reconnect: bool,
    meter: bool,
    error: Option<String>,
}

const FORM_FIELDS: usize = 7;
const FIELD_NAME: usize = 0;
const FIELD_HOST: usize = 1;
const FIELD_KIND: usize = 2;
const FIELD_LISTEN: usize = 3;
const FIELD_TARGET: usize = 4;
const FIELD_AUTO: usize = 5;
const FIELD_METER: usize = 6;

impl Form {
    fn empty() -> Self {
        Self {
            editing: None,
            field: FIELD_NAME,
            name: String::new(),
            host: String::new(),
            kind: TunnelKind::Local,
            listen: String::new(),
            target: String::new(),
            auto_reconnect: true,
            meter: true,
            error: None,
        }
    }

    fn from_spec(spec: &TunnelSpec) -> Self {
        Self {
            editing: Some(spec.name.clone()),
            field: FIELD_NAME,
            name: spec.name.clone(),
            host: spec.host.clone(),
            kind: spec.kind,
            listen: spec.listen.to_string(),
            target: spec.target.as_ref().map(ToString::to_string).unwrap_or_default(),
            auto_reconnect: spec.auto_reconnect,
            meter: spec.meter,
            error: None,
        }
    }

    fn text_field_mut(&mut self) -> Option<&mut String> {
        match self.field {
            FIELD_NAME => Some(&mut self.name),
            FIELD_HOST => Some(&mut self.host),
            FIELD_LISTEN => Some(&mut self.listen),
            FIELD_TARGET => Some(&mut self.target),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
enum Confirm {
    Delete(String),
    Quit,
}

pub struct App {
    should_quit: bool,
    focus: Focus,
    command_mode: bool,
    command_input: String,
    filter_mode: bool,
    filter: String,
    status: String,
    status_kind: StatusKind,
    show_help: bool,
    show_history: bool,
    help_scroll: u16,
    history_scroll: u16,
    log_scroll: usize,
    theme_kind: ThemeKind,
    keymap: Keymap<Intent>,
    specs: Vec<TunnelSpec>,
    ui: HashMap<String, TunnelUi>,
    selected: usize,
    table_state: TableState,
    table_area: Option<Rect>,
    manager: Manager,
    events: tokio::sync::mpsc::UnboundedReceiver<ManagerEvent>,
    history: History,
    hosts: Vec<String>,
    form: Option<Form>,
    confirm: Option<Confirm>,
    pending_reconnect: HashSet<String>,
    last_sample: Instant,
}

impl App {
    pub fn new() -> Self {
        let (manager, events) = Manager::new();
        let (specs, status, status_kind) = match Config::load() {
            Ok(config) => {
                let n = config.tunnels.len();
                (
                    config.tunnels,
                    format!("loaded {n} tunnel(s) from {}", tunnels_path().display()),
                    StatusKind::Info,
                )
            }
            Err(error) => (Vec::new(), error, StatusKind::Error),
        };
        let ui = specs
            .iter()
            .map(|spec| (spec.name.clone(), TunnelUi::default()))
            .collect();

        Self {
            should_quit: false,
            focus: Focus::Table,
            command_mode: false,
            command_input: String::new(),
            filter_mode: false,
            filter: String::new(),
            status,
            status_kind,
            show_help: false,
            show_history: false,
            help_scroll: 0,
            history_scroll: 0,
            log_scroll: 0,
            theme_kind: ThemeKind::Neutral,
            keymap: default_keymap(),
            specs,
            ui,
            selected: 0,
            table_state: TableState::default(),
            table_area: None,
            manager,
            events,
            history: History::load(),
            hosts: ssh_config_hosts(),
            form: None,
            confirm: None,
            pending_reconnect: HashSet::new(),
            last_sample: Instant::now(),
        }
    }

    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    pub async fn shutdown(&mut self) {
        self.manager.shutdown().await;
    }

    fn set_status(&mut self, kind: StatusKind, message: impl Into<String>) {
        self.status = message.into();
        self.status_kind = kind;
    }

    // ---- selection ----------------------------------------------------

    fn visible_indices(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.specs.len()).collect();
        }
        let needle = self.filter.to_lowercase();
        self.specs
            .iter()
            .enumerate()
            .filter(|(_, spec)| {
                spec.name.to_lowercase().contains(&needle)
                    || spec.host.to_lowercase().contains(&needle)
                    || spec.route().to_lowercase().contains(&needle)
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_spec(&self) -> Option<&TunnelSpec> {
        self.visible_indices()
            .get(self.selected)
            .map(|&index| &self.specs[index])
    }

    fn clamp_selection(&mut self) {
        let count = self.visible_indices().len();
        if count == 0 {
            self.selected = 0;
        } else if self.selected >= count {
            self.selected = count - 1;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let count = self.visible_indices().len();
        if count == 0 {
            return;
        }
        let last = count - 1;
        self.selected = self
            .selected
            .saturating_add_signed(delta)
            .min(last);
    }

    // ---- background events ---------------------------------------------

    pub fn on_tick(&mut self) {
        self.poll_events();
        if self.last_sample.elapsed() >= Duration::from_secs(1) {
            self.last_sample = Instant::now();
            self.sample_traffic();
        }
    }

    fn poll_events(&mut self) {
        let mut reconnects: Vec<String> = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            let name = event.name.clone();
            let Some(spec) = self.specs.iter().find(|spec| spec.name == name) else {
                continue;
            };
            let kind = spec.kind;
            let route = spec.route();

            let mut records: Vec<(&'static str, String)> = Vec::new();
            let mut status: Option<(StatusKind, String)> = None;
            let mut reconnect_now = false;

            {
                let ui = self.ui.entry(name.clone()).or_default();
                match event.kind {
                    EventKind::Counters(counters) => {
                        ui.accum_up += ui.prev_up;
                        ui.accum_down += ui.prev_down;
                        ui.prev_up = 0;
                        ui.prev_down = 0;
                        ui.counters = Some(counters);
                    }
                    EventKind::Log(line) => {
                        ui.log.push_back(line);
                        if ui.log.len() > LOG_LINES {
                            ui.log.pop_front();
                        }
                    }
                    EventKind::State(new_state) => {
                        let was_connected = matches!(ui.state, TunnelState::Connected { .. });
                        let now_connected = matches!(new_state, TunnelState::Connected { .. });
                        if now_connected && !was_connected {
                            ui.last_error = None;
                            records.push(("connected", route.clone()));
                            status = Some((StatusKind::Success, format!("{name} connected")));
                        }
                        if was_connected && !now_connected
                            && let TunnelState::Connected { since } = ui.state {
                                let (up, down) = ui.session_totals(kind);
                                records.push((
                                    "disconnected",
                                    format!(
                                        "after {}, ↑ {} ↓ {}",
                                        fmt_duration(since.elapsed()),
                                        human_bytes(up),
                                        human_bytes(down)
                                    ),
                                ));
                            }
                        if let TunnelState::Failed { reason } = &new_state {
                            ui.last_error = Some(reason.clone());
                            records.push(("failed", reason.clone()));
                            status = Some((StatusKind::Error, format!("{name}: {reason}")));
                        }
                        let now_inactive =
                            matches!(new_state, TunnelState::Idle | TunnelState::Failed { .. });
                        ui.state = new_state;
                        if now_inactive {
                            reconnect_now = true;
                        }
                    }
                }
            }

            for (event_name, detail) in records {
                self.history.record(&name, event_name, detail);
            }
            if let Some((kind, message)) = status {
                self.set_status(kind, message);
            }
            if reconnect_now && self.pending_reconnect.remove(&name) {
                reconnects.push(name);
            }
        }
        for name in reconnects {
            self.do_connect(&name);
        }
    }

    fn sample_traffic(&mut self) {
        let kinds: Vec<(String, TunnelKind)> = self
            .specs
            .iter()
            .map(|spec| (spec.name.clone(), spec.kind))
            .collect();
        for (name, kind) in kinds {
            let Some(ui) = self.ui.get_mut(&name) else {
                continue;
            };
            let (up, down) = ui.session_totals(kind);
            ui.up_rate = up.saturating_sub(ui.prev_up);
            ui.down_rate = down.saturating_sub(ui.prev_down);
            ui.prev_up = up;
            ui.prev_down = down;
            ui.up_hist.push_back(ui.up_rate);
            ui.down_hist.push_back(ui.down_rate);
            while ui.up_hist.len() > RATE_HISTORY {
                ui.up_hist.pop_front();
            }
            while ui.down_hist.len() > RATE_HISTORY {
                ui.down_hist.pop_front();
            }
        }
    }

    // ---- input ---------------------------------------------------------

    pub fn handle_key_event(&mut self, key: KeyEvent) {
        if self.command_mode {
            self.handle_command_key(key);
            return;
        }
        if self.confirm.is_some() {
            self.handle_confirm_key(key);
            return;
        }
        if self.form.is_some() {
            self.handle_form_key(key);
            return;
        }
        if self.filter_mode {
            self.handle_filter_key(key);
            return;
        }
        if let Some(intent) = self.keymap.intent_for(key) {
            self.handle_intent(intent);
        }
    }

    fn handle_intent(&mut self, intent: Intent) {
        if self.show_help || self.show_history {
            self.handle_overlay_intent(intent);
            return;
        }

        match intent {
            Intent::EnterCommandMode => self.enter_command_mode(),
            Intent::Cancel => self.cancel_transient_state(),
            Intent::Help => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            Intent::History => {
                self.show_history = true;
                self.history_scroll = 0;
            }
            Intent::NextPane | Intent::PreviousPane => {
                self.focus = match self.focus {
                    Focus::Table => Focus::Detail,
                    Focus::Detail => Focus::Table,
                };
            }
            Intent::Activate => self.toggle_selected(),
            Intent::MoveUp => match self.focus {
                Focus::Table => self.move_selection(-1),
                Focus::Detail => self.log_scroll = self.log_scroll.saturating_add(1),
            },
            Intent::MoveDown => match self.focus {
                Focus::Table => self.move_selection(1),
                Focus::Detail => self.log_scroll = self.log_scroll.saturating_sub(1),
            },
            Intent::PageUp => self.move_selection(-10),
            Intent::PageDown => self.move_selection(10),
            Intent::Top => self.selected = 0,
            Intent::Bottom => {
                let count = self.visible_indices().len();
                self.selected = count.saturating_sub(1);
            }
            Intent::Search => {
                self.filter_mode = true;
                self.set_status(StatusKind::Info, "filter: type to match name/host, Enter to keep, Esc to clear");
            }
            Intent::Connect => self.cmd_connect(Target::Selected),
            Intent::Disconnect => self.cmd_disconnect(Target::Selected),
            Intent::Reconnect => self.cmd_reconnect(Target::Selected),
            Intent::ConnectAll => self.cmd_connect(Target::All),
            Intent::DisconnectAll => self.cmd_disconnect(Target::All),
            Intent::Add => self.open_add_form(),
            Intent::Edit => self.open_edit_form(None),
            Intent::Delete => self.request_delete(None),
        }
    }

    fn handle_overlay_intent(&mut self, intent: Intent) {
        match intent {
            Intent::Cancel => {
                self.show_help = false;
                self.show_history = false;
            }
            Intent::Help if self.show_help => self.show_help = false,
            Intent::History if self.show_history => self.show_history = false,
            Intent::MoveUp => self.scroll_overlay(-1),
            Intent::MoveDown => self.scroll_overlay(1),
            Intent::PageUp => self.scroll_overlay(-10),
            Intent::PageDown => self.scroll_overlay(10),
            Intent::Top => self.scroll_overlay(i16::MIN),
            Intent::EnterCommandMode => {
                self.show_help = false;
                self.show_history = false;
                self.enter_command_mode();
            }
            _ => {}
        }
    }

    fn scroll_overlay(&mut self, delta: i16) {
        let scroll = if self.show_help {
            &mut self.help_scroll
        } else {
            &mut self.history_scroll
        };
        if delta == i16::MIN {
            *scroll = 0;
        } else {
            *scroll = scroll.saturating_add_signed(delta);
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.cancel_transient_state(),
            KeyCode::Enter => self.submit_command(),
            KeyCode::Backspace => {
                self.command_input.pop();
                if self.command_input.is_empty() {
                    self.command_mode = false;
                }
            }
            KeyCode::Char(character) if text_input_modifiers(key.modifiers) => {
                self.command_input.push(character);
            }
            _ => {}
        }
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.filter.clear();
                self.filter_mode = false;
                self.clamp_selection();
                self.set_status(StatusKind::Info, "filter cleared");
            }
            KeyCode::Enter => {
                self.filter_mode = false;
                self.set_status(
                    StatusKind::Info,
                    if self.filter.is_empty() {
                        "filter cleared".to_string()
                    } else {
                        format!("filter: {}", self.filter)
                    },
                );
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.clamp_selection();
            }
            KeyCode::Char(character) if text_input_modifiers(key.modifiers) => {
                self.filter.push(character);
                self.selected = 0;
            }
            _ => {}
        }
    }

    fn handle_confirm_key(&mut self, key: KeyEvent) {
        let confirmed = matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y'));
        let cancelled = matches!(
            key.code,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc
        );
        if !confirmed && !cancelled {
            return;
        }
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        if cancelled {
            self.set_status(StatusKind::Info, "canceled");
            return;
        }
        match confirm {
            Confirm::Delete(name) => self.delete_tunnel(&name),
            Confirm::Quit => {
                for name in self.manager.running_names() {
                    self.manager.disconnect(&name);
                }
                self.should_quit = true;
            }
        }
    }

    fn handle_form_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.form = None;
                self.set_status(StatusKind::Info, "form canceled");
                return;
            }
            KeyCode::Enter => {
                self.submit_form();
                return;
            }
            _ => {}
        }
        let Some(form) = self.form.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Up | KeyCode::BackTab => {
                form.field = (form.field + FORM_FIELDS - 1) % FORM_FIELDS;
            }
            KeyCode::Down | KeyCode::Tab => {
                form.field = (form.field + 1) % FORM_FIELDS;
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char(' ')
                if matches!(form.field, FIELD_KIND | FIELD_AUTO | FIELD_METER) =>
            {
                match form.field {
                    FIELD_KIND => form.kind = form.kind.next(),
                    FIELD_AUTO => form.auto_reconnect = !form.auto_reconnect,
                    FIELD_METER => form.meter = !form.meter,
                    _ => {}
                }
            }
            KeyCode::Backspace => {
                if let Some(text) = form.text_field_mut() {
                    text.pop();
                }
            }
            KeyCode::Char(character) if text_input_modifiers(key.modifiers) => {
                if let Some(text) = form.text_field_mut() {
                    text.push(character);
                }
            }
            _ => {}
        }
    }

    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        if self.command_mode || self.form.is_some() || self.confirm.is_some() {
            return;
        }
        if self.show_help || self.show_history {
            match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_overlay(-2),
                MouseEventKind::ScrollDown => self.scroll_overlay(2),
                _ => {}
            }
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_selection(-1),
            MouseEventKind::ScrollDown => self.move_selection(1),
            MouseEventKind::Down(_) => {
                let Some(area) = self.table_area else {
                    return;
                };
                if mouse.column < area.x
                    || mouse.column >= area.x + area.width
                    || mouse.row < area.y + 2
                    || mouse.row >= area.y + area.height.saturating_sub(1)
                {
                    return;
                }
                // block border (1) + header row (1)
                let row = (mouse.row - area.y - 2) as usize + self.table_state.offset();
                if row < self.visible_indices().len() {
                    self.selected = row;
                    self.focus = Focus::Table;
                }
            }
            _ => {}
        }
    }

    // ---- commands --------------------------------------------------------

    fn enter_command_mode(&mut self) {
        self.command_mode = true;
        self.command_input.clear();
        self.command_input.push(':');
    }

    fn cancel_transient_state(&mut self) {
        if self.command_mode {
            self.command_mode = false;
            self.command_input.clear();
            return;
        }
        if !self.filter.is_empty() {
            self.filter.clear();
            self.clamp_selection();
            self.set_status(StatusKind::Info, "filter cleared");
            return;
        }
        self.set_status(StatusKind::Info, "nothing to cancel (:q quits)");
    }

    fn submit_command(&mut self) {
        let input = self.command_input.clone();
        self.command_mode = false;
        self.command_input.clear();

        match command::parse(&input) {
            Command::Quit { force } => self.request_quit(force),
            Command::Help => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            Command::History => {
                self.show_history = true;
                self.history_scroll = 0;
            }
            Command::Add => self.open_add_form(),
            Command::Reload => self.reload_config(),
            Command::Theme(theme) => self.set_theme(theme),
            Command::Connect(target) => self.cmd_connect(target),
            Command::Disconnect(target) => self.cmd_disconnect(target),
            Command::Reconnect(target) => self.cmd_reconnect(target),
            Command::Edit(name) => self.open_edit_form(name),
            Command::Delete(name) => self.request_delete(name),
            Command::Clean(name) => self.do_clean(name),
            Command::Unknown(text) => {
                self.set_status(StatusKind::Warning, format!("unknown command: {text}"));
            }
            Command::Empty => {}
        }
    }

    fn resolve_target(&mut self, target: Target) -> Vec<String> {
        match target {
            Target::All => self.specs.iter().map(|spec| spec.name.clone()).collect(),
            Target::Selected => match self.selected_spec() {
                Some(spec) => vec![spec.name.clone()],
                None => {
                    self.set_status(StatusKind::Warning, "no tunnel selected");
                    Vec::new()
                }
            },
            Target::Named(name) => {
                if self.specs.iter().any(|spec| spec.name == name) {
                    vec![name]
                } else {
                    self.set_status(StatusKind::Warning, format!("no tunnel named `{name}`"));
                    Vec::new()
                }
            }
        }
    }

    fn resolve_name(&mut self, name: Option<String>) -> Option<String> {
        match name {
            Some(name) => {
                if self.specs.iter().any(|spec| spec.name == name) {
                    Some(name)
                } else {
                    self.set_status(StatusKind::Warning, format!("no tunnel named `{name}`"));
                    None
                }
            }
            None => {
                let selected = self.selected_spec().map(|spec| spec.name.clone());
                if selected.is_none() {
                    self.set_status(StatusKind::Warning, "no tunnel selected");
                }
                selected
            }
        }
    }

    fn cmd_connect(&mut self, target: Target) {
        for name in self.resolve_target(target) {
            if !self.manager.is_running(&name) {
                self.do_connect(&name);
            }
        }
    }

    fn cmd_disconnect(&mut self, target: Target) {
        for name in self.resolve_target(target) {
            self.do_disconnect(&name);
        }
    }

    fn cmd_reconnect(&mut self, target: Target) {
        for name in self.resolve_target(target) {
            if self.manager.is_running(&name) {
                self.pending_reconnect.insert(name.clone());
                self.manager.disconnect(&name);
                self.history.record(&name, "reconnect", "");
                self.set_status(StatusKind::Info, format!("reconnecting {name} …"));
            } else {
                self.do_connect(&name);
            }
        }
    }

    fn do_connect(&mut self, name: &str) {
        let Some(spec) = self.specs.iter().find(|spec| spec.name == name).cloned() else {
            self.set_status(StatusKind::Warning, format!("no tunnel named `{name}`"));
            return;
        };
        if let Err(error) = spec.validate() {
            self.set_status(StatusKind::Error, format!("{name}: {error}"));
            return;
        }
        let route = spec.route();
        match self.manager.connect(spec) {
            Ok(()) => {
                let ui = self.ui.entry(name.to_string()).or_default();
                ui.state = TunnelState::Connecting;
                ui.last_error = None;
                self.history.record(name, "connect", route);
                self.set_status(StatusKind::Info, format!("connecting {name} …"));
            }
            Err(error) => self.set_status(StatusKind::Warning, error),
        }
    }

    fn do_disconnect(&mut self, name: &str) {
        if self.manager.disconnect(name) {
            self.history.record(name, "disconnect", "");
            self.set_status(StatusKind::Info, format!("disconnecting {name} …"));
        } else {
            self.set_status(StatusKind::Warning, format!("{name} is not running"));
        }
    }

    fn toggle_selected(&mut self) {
        let Some(spec) = self.selected_spec() else {
            return;
        };
        let name = spec.name.clone();
        if self.manager.is_running(&name) {
            self.do_disconnect(&name);
        } else {
            self.do_connect(&name);
        }
    }

    fn request_quit(&mut self, force: bool) {
        let active = self.manager.running_names().len();
        if force || active == 0 {
            self.should_quit = true;
        } else {
            self.confirm = Some(Confirm::Quit);
        }
    }

    fn request_delete(&mut self, name: Option<String>) {
        if let Some(name) = self.resolve_name(name) {
            self.confirm = Some(Confirm::Delete(name));
        }
    }

    fn delete_tunnel(&mut self, name: &str) {
        self.manager.disconnect(name);
        self.specs.retain(|spec| spec.name != name);
        self.ui.remove(name);
        self.pending_reconnect.remove(name);
        self.clamp_selection();
        self.save_config();
        self.history.record(name, "deleted", "");
        self.set_status(StatusKind::Info, format!("deleted {name}"));
    }

    fn open_add_form(&mut self) {
        self.form = Some(Form::empty());
    }

    fn open_edit_form(&mut self, name: Option<String>) {
        let Some(name) = self.resolve_name(name) else {
            return;
        };
        if self.manager.is_running(&name) {
            self.set_status(
                StatusKind::Warning,
                format!("disconnect {name} before editing it"),
            );
            return;
        }
        if let Some(spec) = self.specs.iter().find(|spec| spec.name == name) {
            self.form = Some(Form::from_spec(spec));
        }
    }

    fn submit_form(&mut self) {
        let Some(form) = self.form.clone() else {
            return;
        };
        match self.build_spec_from_form(&form) {
            Ok(spec) => {
                let name = spec.name.clone();
                if let Some(original) = &form.editing {
                    if let Some(slot) =
                        self.specs.iter_mut().find(|spec| &spec.name == original)
                    {
                        *slot = spec;
                    }
                    if original != &name
                        && let Some(ui) = self.ui.remove(original) {
                            self.ui.insert(name.clone(), ui);
                        }
                    self.history.record(&name, "edited", "");
                    self.set_status(StatusKind::Success, format!("saved {name}"));
                } else {
                    self.specs.push(spec);
                    self.ui.insert(name.clone(), TunnelUi::default());
                    self.history.record(&name, "added", "");
                    self.set_status(StatusKind::Success, format!("added {name}"));
                }
                self.form = None;
                self.save_config();
            }
            Err(error) => {
                if let Some(form) = self.form.as_mut() {
                    form.error = Some(error);
                }
            }
        }
    }

    fn build_spec_from_form(&self, form: &Form) -> Result<TunnelSpec, String> {
        let name = form.name.trim().to_string();
        if name.is_empty() {
            return Err("name is required".to_string());
        }
        let taken = self
            .specs
            .iter()
            .any(|spec| spec.name == name && form.editing.as_deref() != Some(spec.name.as_str()));
        if taken {
            return Err(format!("a tunnel named `{name}` already exists"));
        }
        let listen: Endpoint = form
            .listen
            .parse()
            .map_err(|error| format!("listen: {error}"))?;
        let target: Option<Endpoint> = if form.kind == TunnelKind::Dynamic {
            None
        } else {
            Some(
                form.target
                    .parse()
                    .map_err(|error| format!("target: {error}"))?,
            )
        };
        let spec = TunnelSpec {
            name,
            host: form.host.trim().to_string(),
            kind: form.kind,
            listen,
            target,
            auto_reconnect: form.auto_reconnect,
            meter: form.meter,
            extra_ssh_args: form
                .editing
                .as_ref()
                .and_then(|original| self.specs.iter().find(|spec| &spec.name == original))
                .map(|spec| spec.extra_ssh_args.clone())
                .unwrap_or_default(),
        };
        spec.validate()?;
        Ok(spec)
    }

    fn save_config(&mut self) {
        let config = Config { tunnels: self.specs.clone() };
        if let Err(error) = config.save() {
            self.set_status(StatusKind::Error, error);
        }
    }

    fn reload_config(&mut self) {
        match Config::load() {
            Ok(config) => {
                self.specs = config.tunnels;
                self.ui.retain(|name, _| {
                    self.specs.iter().any(|spec| &spec.name == name)
                });
                for spec in &self.specs {
                    self.ui.entry(spec.name.clone()).or_default();
                }
                self.clamp_selection();
                self.set_status(
                    StatusKind::Info,
                    format!("reloaded {} tunnel(s)", self.specs.len()),
                );
            }
            Err(error) => self.set_status(StatusKind::Error, error),
        }
    }

    fn do_clean(&mut self, name: Option<String>) {
        let Some(name) = self.resolve_name(name) else {
            return;
        };
        let Some(spec) = self.specs.iter().find(|spec| spec.name == name).cloned() else {
            return;
        };
        if self.manager.is_running(&name) {
            self.set_status(
                StatusKind::Warning,
                format!("disconnect {name} before cleaning its socket"),
            );
            return;
        }
        if let Some(path) = spec.local_unix_socket() {
            let expanded = expand_tilde(path);
            if !expanded.exists() {
                self.set_status(
                    StatusKind::Info,
                    format!("nothing to clean: {} does not exist", expanded.display()),
                );
                return;
            }
            match relay::remove_stale_socket(&expanded) {
                Ok(()) => {
                    self.history.record(&name, "clean", expanded.display().to_string());
                    self.set_status(
                        StatusKind::Success,
                        format!("removed {}", expanded.display()),
                    );
                }
                Err(error) => self.set_status(StatusKind::Error, error.to_string()),
            }
        } else if spec.remote_unix_socket().is_some() {
            match self.manager.clean_remote(&spec) {
                Ok(()) => {
                    self.history.record(&name, "clean", format!("remote {}", spec.listen));
                    self.set_status(
                        StatusKind::Info,
                        format!("removing {} on {} over ssh …", spec.listen, spec.host),
                    );
                }
                Err(error) => self.set_status(StatusKind::Warning, error),
            }
        } else {
            self.set_status(
                StatusKind::Info,
                format!("{name} has no unix socket to clean"),
            );
        }
    }

    fn set_theme(&mut self, theme: ThemeCommand) {
        self.theme_kind = match theme {
            ThemeCommand::Neutral => ThemeKind::Neutral,
            ThemeCommand::Safe => ThemeKind::Safe,
            ThemeCommand::Danger => ThemeKind::Danger,
        };
        self.set_status(StatusKind::Info, format!("theme: {}", self.theme().name));
    }

    fn theme(&self) -> Theme {
        self.theme_kind.theme()
    }

    // ---- rendering -------------------------------------------------------

    pub fn render(&mut self, frame: &mut Frame<'_>) {
        self.clamp_selection();
        let theme = self.theme();
        let area = frame.area();
        frame.render_widget(Clear, area);
        frame.render_widget(
            Block::default().style(theme.style(Role::AppBackground)),
            area,
        );

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(6),
                Constraint::Length(12),
                Constraint::Length(4),
            ])
            .split(area);

        self.render_header(frame, rows[0], &theme);
        self.render_table(frame, rows[1], &theme);
        self.render_detail(frame, rows[2], &theme);
        self.render_command(frame, rows[3], &theme);

        if self.show_help {
            self.render_help(frame, centered_rect(area, 72, 80), &theme);
        }
        if self.show_history {
            self.render_history(frame, centered_rect(area, 84, 80), &theme);
        }
        if self.form.is_some() {
            self.render_form(frame, centered_rect(area, 64, 62), &theme);
        }
        if self.confirm.is_some() {
            self.render_confirm(frame, centered_rect(area, 54, 22), &theme);
        }
    }

    fn state_of(&self, name: &str) -> TunnelState {
        self.ui
            .get(name)
            .map(|ui| ui.state.clone())
            .unwrap_or_default()
    }

    fn render_header(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let mut connected = 0usize;
        let mut connecting = 0usize;
        let mut failed = 0usize;
        let mut idle = 0usize;
        let mut up_rate = 0u64;
        let mut down_rate = 0u64;
        for spec in &self.specs {
            match self.state_of(&spec.name) {
                TunnelState::Connected { .. } => connected += 1,
                TunnelState::Connecting | TunnelState::Reconnecting { .. } => connecting += 1,
                TunnelState::Failed { .. } => failed += 1,
                TunnelState::Idle => idle += 1,
            }
            if let Some(ui) = self.ui.get(&spec.name) {
                up_rate += ui.up_rate;
                down_rate += ui.down_rate;
            }
        }

        let mut spans = vec![
            Span::styled(" rumole ", theme.style(Role::HeaderBrand)),
            Span::styled(" ssh tunnels ", theme.style(Role::HeaderMode)),
            Span::raw("  "),
            Span::styled(
                format!("● {connected} up"),
                theme.selector("state.connected"),
            ),
            Span::raw("  "),
        ];
        if connecting > 0 {
            spans.push(Span::styled(
                format!("◌ {connecting} connecting"),
                theme.selector("state.connecting"),
            ));
            spans.push(Span::raw("  "));
        }
        if failed > 0 {
            spans.push(Span::styled(
                format!("✕ {failed} failed"),
                theme.selector("state.failed"),
            ));
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(
            format!("○ {idle} idle"),
            theme.selector("state.idle"),
        ));
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("↑ {}", human_rate(up_rate)),
            theme.selector("traffic.up"),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("↓ {}", human_rate(down_rate)),
            theme.selector("traffic.down"),
        ));

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.border))
            .style(theme.style(Role::Header));
        frame.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
    }

    fn render_table(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let visible = self.visible_indices();
        let title = if self.filter.is_empty() {
            format!("Tunnels ({})", visible.len())
        } else {
            format!("Tunnels ({}) — filter: {}", visible.len(), self.filter)
        };
        let block = panel_block(title, self.focus == Focus::Table, theme);

        if self.specs.is_empty() {
            let text = Paragraph::new(vec![
                Line::from(""),
                Line::from("  No tunnels configured."),
                Line::from(""),
                Line::from(vec![
                    Span::raw("  Press "),
                    Span::styled("a", theme.style(Role::FooterKey)),
                    Span::raw(" to add one, or edit "),
                    Span::raw(tunnels_path().display().to_string()),
                ]),
            ])
            .block(block)
            .style(theme.style(Role::Panel));
            frame.render_widget(text, area);
            self.table_area = Some(area);
            return;
        }

        let header = Row::new(["", "NAME", "T", "ROUTE", "CONN", "↑ RATE", "↓ RATE", "UPTIME"])
            .style(theme.style(Role::TableHeader));
        let rows: Vec<Row<'_>> = visible
            .iter()
            .map(|&index| {
                let spec = &self.specs[index];
                let state = self.state_of(&spec.name);
                let ui = self.ui.get(&spec.name);
                let conns = ui
                    .and_then(|ui| ui.connections())
                    .map(|(active, total)| format!("{active}/{total}"))
                    .unwrap_or_else(|| "–".to_string());
                let (up, down) = ui
                    .map(|ui| (ui.up_rate, ui.down_rate))
                    .unwrap_or_default();
                let rates_known = spec.meter && state.is_active();
                let uptime = match &state {
                    TunnelState::Connected { since } => fmt_duration(since.elapsed()),
                    TunnelState::Reconnecting { retry_at, attempt } => {
                        let wait = retry_at.saturating_duration_since(Instant::now());
                        format!("retry {}s #{attempt}", wait.as_secs())
                    }
                    _ => "–".to_string(),
                };
                let dim = if state.is_active() {
                    Style::default()
                } else {
                    Style::default().add_modifier(Modifier::DIM)
                };
                Row::new(vec![
                    Cell::from(Span::styled(state.glyph(), state_style(&state, theme))),
                    Cell::from(Span::styled(spec.name.clone(), dim.add_modifier(Modifier::BOLD))),
                    Cell::from(Span::styled(spec.kind.short(), dim)),
                    Cell::from(route_line(spec, theme).style(dim)),
                    Cell::from(Span::styled(conns, dim)),
                    Cell::from(Span::styled(
                        if rates_known { human_rate(up) } else { "–".to_string() },
                        theme.selector("traffic.up").patch(dim),
                    )),
                    Cell::from(Span::styled(
                        if rates_known { human_rate(down) } else { "–".to_string() },
                        theme.selector("traffic.down").patch(dim),
                    )),
                    Cell::from(Span::styled(uptime, dim)),
                ])
            })
            .collect();

        let widths = [
            Constraint::Length(1),
            Constraint::Length(16),
            Constraint::Length(1),
            Constraint::Min(28),
            Constraint::Length(7),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(12),
        ];
        let table = Table::new(rows, widths)
            .header(header)
            .block(block)
            .column_spacing(1)
            .style(theme.style(Role::Panel))
            .row_highlight_style(theme.style(Role::ListItemSelected));

        self.table_state
            .select((!visible.is_empty()).then_some(self.selected));
        frame.render_stateful_widget(table, area, &mut self.table_state);
        self.table_area = Some(area);
    }

    fn render_detail(&mut self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let Some(spec) = self.selected_spec().cloned() else {
            let block = panel_block("Detail".to_string(), self.focus == Focus::Detail, theme);
            frame.render_widget(
                Paragraph::new("select a tunnel")
                    .block(block)
                    .style(theme.style(Role::TextMuted)),
                area,
            );
            return;
        };
        let title = format!("Detail — {}", spec.name);
        let block = panel_block(title, self.focus == Focus::Detail, theme);
        let inner = block.inner(area);
        frame.render_widget(block.style(theme.style(Role::Panel)), area);

        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(44), Constraint::Length(30)])
            .split(inner);

        self.render_detail_info(frame, columns[0], theme, &spec);
        self.render_detail_traffic(frame, columns[1], theme, &spec);
    }

    fn render_detail_info(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        spec: &TunnelSpec,
    ) {
        let state = self.state_of(&spec.name);
        let ui = self.ui.get(&spec.name);

        let mut lines: Vec<Line<'_>> = Vec::new();
        lines.push(diagram_line(spec, &state, theme));

        // state line
        let mut state_spans = vec![
            Span::styled(
                format!("{} {}", state.glyph(), state.word()),
                state_style(&state, theme),
            ),
        ];
        match &state {
            TunnelState::Connected { since } => {
                state_spans.push(Span::styled(
                    format!("  up {}", fmt_duration(since.elapsed())),
                    theme.style(Role::Text),
                ));
            }
            TunnelState::Reconnecting { attempt, retry_at } => {
                let wait = retry_at.saturating_duration_since(Instant::now());
                state_spans.push(Span::styled(
                    format!("  attempt {attempt}, retrying in {}s", wait.as_secs()),
                    theme.style(Role::TextMuted),
                ));
            }
            _ => {}
        }
        if let Some(error) = ui.and_then(|ui| ui.last_error.as_ref()) {
            state_spans.push(Span::styled(
                format!("  {error}"),
                theme.style(Role::StatusDanger),
            ));
        }
        lines.push(Line::from(state_spans));

        // options line
        lines.push(Line::from(vec![
            Span::styled(spec.kind.label(), theme.style(Role::Text)),
            Span::styled(
                format!(
                    "  ·  auto-reconnect {}  ·  meter {}",
                    if spec.auto_reconnect { "on" } else { "off" },
                    if spec.meter { "on" } else { "off" },
                ),
                theme.style(Role::TextMuted),
            ),
        ]));

        // exact ssh command, for explicitness; the relay port is chosen at
        // connect time, so show a placeholder when metering is on
        let ssh_preview = spec
            .ssh_args(spec.meter.then_some(0))
            .join(" ")
            .replace("127.0.0.1:0", "127.0.0.1:<relay>");
        lines.push(Line::from(Span::styled(
            format!("$ ssh {ssh_preview}"),
            theme.style(Role::TextMuted),
        )));

        // stale socket warning
        if !state.is_active()
            && let Some(path) = spec.local_unix_socket()
                && expand_tilde(path).exists() {
                    lines.push(Line::from(Span::styled(
                        format!("⚠ socket file {path} still exists — :clean removes it"),
                        theme.style(Role::StatusWarning),
                    )));
                }

        // log tail
        if let Some(ui) = ui {
            let room = (area.height as usize).saturating_sub(lines.len());
            if room > 0 && !ui.log.is_empty() {
                let total = ui.log.len();
                let end = total.saturating_sub(self.log_scroll.min(total.saturating_sub(1)));
                let start = end.saturating_sub(room);
                for line in ui.log.iter().skip(start).take(end - start) {
                    lines.push(Line::from(Span::styled(
                        format!("│ {line}"),
                        theme.style(Role::TextMuted),
                    )));
                }
            }
        }

        frame.render_widget(
            Paragraph::new(lines).style(theme.style(Role::Panel)),
            area,
        );
    }

    fn render_detail_traffic(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        theme: &Theme,
        spec: &TunnelSpec,
    ) {
        let Some(ui) = self.ui.get(&spec.name) else {
            return;
        };
        if !spec.meter {
            frame.render_widget(
                Paragraph::new("metering off")
                    .style(theme.style(Role::TextMuted))
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        }
        let (total_up, total_down) = ui.totals(spec.kind);
        let conns = ui
            .connections()
            .map(|(active, total)| format!("{active} active / {total} total"))
            .unwrap_or_else(|| "no session".to_string());

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(2),
                Constraint::Length(1),
                Constraint::Length(2),
                Constraint::Min(0),
            ])
            .split(area);

        frame.render_widget(
            Paragraph::new(Span::styled(
                format!("conns {conns}"),
                theme.style(Role::TextMuted),
            )),
            rows[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("↑ {}", human_rate(ui.up_rate)),
                    theme.selector("traffic.up"),
                ),
                Span::styled(
                    format!("  Σ {}", human_bytes(total_up)),
                    theme.style(Role::TextMuted),
                ),
            ])),
            rows[1],
        );
        frame.render_widget(
            sparkline(&ui.up_hist, rows[2].width, theme.selector("traffic.up")),
            rows[2],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    format!("↓ {}", human_rate(ui.down_rate)),
                    theme.selector("traffic.down"),
                ),
                Span::styled(
                    format!("  Σ {}", human_bytes(total_down)),
                    theme.style(Role::TextMuted),
                ),
            ])),
            rows[3],
        );
        frame.render_widget(
            sparkline(&ui.down_hist, rows[4].width, theme.selector("traffic.down")),
            rows[4],
        );
    }

    fn render_command(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let prompt = if self.command_mode {
            Line::from(vec![
                Span::styled("cmd ", theme.style(Role::PromptPrefix)),
                Span::raw(self.command_input.clone()),
                Span::styled("▏", theme.style(Role::PromptPrefix)),
            ])
        } else if self.filter_mode {
            Line::from(vec![
                Span::styled("filter /", theme.style(Role::PromptPrefix)),
                Span::raw(self.filter.clone()),
                Span::styled("▏", theme.style(Role::PromptPrefix)),
            ])
        } else {
            let status_style = match self.status_kind {
                StatusKind::Info => theme.style(Role::Status),
                StatusKind::Success => theme.style(Role::StatusSuccess),
                StatusKind::Warning => theme.style(Role::StatusWarning),
                StatusKind::Error => theme.style(Role::StatusDanger),
            };
            Line::from(vec![Span::styled(self.status.clone(), status_style)])
        };

        let footer = menu::footer(&self.keymap, theme);
        let block = panel_block("Command".to_string(), self.command_mode, theme);
        frame.render_widget(
            Paragraph::new(vec![prompt, footer])
                .block(block)
                .style(theme.style(Role::Command)),
            area,
        );
    }

    fn render_help(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .title("Help")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .style(theme.style(Role::HelpOverlay));
        frame.render_widget(
            Paragraph::new(menu::help_lines(&self.keymap))
                .block(block)
                .scroll((self.help_scroll, 0))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn render_history(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        frame.render_widget(Clear, area);
        let title = format!("History — {}", self.history.path().display());
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .style(theme.style(Role::HelpOverlay));

        let mut lines: Vec<Line<'_>> = Vec::new();
        for entry in self.history.entries().iter().rev() {
            let event_style = match entry.event.as_str() {
                "connected" => theme.selector("state.connected"),
                "failed" => theme.selector("state.failed"),
                "disconnected" | "disconnect" => theme.selector("state.idle"),
                _ => theme.style(Role::Text),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    entry.at.format("%m-%d %H:%M:%S ").to_string(),
                    theme.style(Role::TextMuted),
                ),
                Span::styled(format!("{:<14}", entry.tunnel), theme.style(Role::Text)),
                Span::styled(format!("{:<13}", entry.event), event_style),
                Span::styled(entry.detail.clone(), theme.style(Role::TextMuted)),
            ]));
        }
        if lines.is_empty() {
            lines.push(Line::from("no events yet"));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .scroll((self.history_scroll, 0)),
            area,
        );
    }

    fn render_form(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let Some(form) = &self.form else {
            return;
        };
        frame.render_widget(Clear, area);
        let title = if form.editing.is_some() { "Edit tunnel" } else { "Add tunnel" };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .style(theme.style(Role::HelpOverlay));

        let field_style = |field: usize| {
            if form.field == field {
                theme.style(Role::InputFocused).add_modifier(Modifier::BOLD)
            } else {
                theme.style(Role::Input)
            }
        };
        let text_value = |field: usize, value: &str| {
            if form.field == field {
                format!("{value}▏")
            } else if value.is_empty() {
                " ".to_string()
            } else {
                value.to_string()
            }
        };
        let label = |text: &str| Span::styled(format!(" {text:<16}"), theme.style(Role::Text));

        let mut lines = vec![
            Line::from(""),
            Line::from(vec![
                label("Name"),
                Span::styled(text_value(FIELD_NAME, &form.name), field_style(FIELD_NAME)),
            ]),
            Line::from(vec![
                label("SSH host"),
                Span::styled(text_value(FIELD_HOST, &form.host), field_style(FIELD_HOST)),
            ]),
        ];
        if form.field == FIELD_HOST && !self.hosts.is_empty() {
            let matches: Vec<&str> = self
                .hosts
                .iter()
                .filter(|host| host.starts_with(form.host.as_str()))
                .take(5)
                .map(String::as_str)
                .collect();
            if !matches.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("                  ~/.ssh/config: {}", matches.join(", ")),
                    theme.style(Role::TextMuted),
                )));
            }
        }
        lines.push(Line::from(vec![
            label("Kind"),
            Span::styled(
                format!("◂ {} ▸", form.kind.label()),
                field_style(FIELD_KIND),
            ),
        ]));
        lines.push(Line::from(vec![
            label("Listen"),
            Span::styled(
                text_value(FIELD_LISTEN, &form.listen),
                field_style(FIELD_LISTEN),
            ),
            Span::styled(
                match form.kind {
                    TunnelKind::Remote => "  (on remote host)",
                    _ => "  (on this machine)",
                },
                theme.style(Role::TextMuted),
            ),
        ]));
        if form.kind == TunnelKind::Dynamic {
            lines.push(Line::from(vec![
                label("Target"),
                Span::styled("(none — SOCKS proxy)", theme.style(Role::TextMuted)),
            ]));
        } else {
            lines.push(Line::from(vec![
                label("Target"),
                Span::styled(
                    text_value(FIELD_TARGET, &form.target),
                    field_style(FIELD_TARGET),
                ),
                Span::styled(
                    match form.kind {
                        TunnelKind::Remote => "  (on this machine)",
                        _ => "  (as seen from remote)",
                    },
                    theme.style(Role::TextMuted),
                ),
            ]));
        }
        lines.push(Line::from(vec![
            label("Auto-reconnect"),
            Span::styled(
                if form.auto_reconnect { "[x]" } else { "[ ]" },
                field_style(FIELD_AUTO),
            ),
        ]));
        lines.push(Line::from(vec![
            label("Meter traffic"),
            Span::styled(
                if form.meter { "[x]" } else { "[ ]" },
                field_style(FIELD_METER),
            ),
        ]));
        lines.push(Line::from(""));
        if let Some(error) = &form.error {
            lines.push(Line::from(Span::styled(
                format!(" {error}"),
                theme.style(Role::StatusDanger),
            )));
        }
        lines.push(Line::from(Span::styled(
            " Enter save · Esc cancel · ↑↓/Tab fields · Space/←→ change",
            theme.style(Role::TextMuted),
        )));

        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_confirm(&self, frame: &mut Frame<'_>, area: Rect, theme: &Theme) {
        let Some(confirm) = &self.confirm else {
            return;
        };
        frame.render_widget(Clear, area);
        let (title, message) = match confirm {
            Confirm::Delete(name) => (
                "Delete tunnel",
                format!("Delete `{name}`? A running session will be disconnected."),
            ),
            Confirm::Quit => (
                "Quit",
                format!(
                    "{} tunnel(s) still connected. Disconnect and quit?",
                    self.manager.running_names().len()
                ),
            ),
        };
        let block = Block::default()
            .title(title)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.danger))
            .style(theme.style(Role::HelpOverlay));
        let lines = vec![
            Line::from(""),
            Line::from(format!(" {message}")),
            Line::from(""),
            Line::from(vec![
                Span::raw(" "),
                Span::styled(" y ", theme.style(Role::ButtonDanger)),
                Span::raw(" confirm   "),
                Span::styled(" n ", theme.style(Role::Button)),
                Span::raw(" cancel"),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(lines).block(block).wrap(Wrap { trim: false }),
            area,
        );
    }
}

fn state_style(state: &TunnelState, theme: &Theme) -> Style {
    match state {
        TunnelState::Connected { .. } => theme.selector("state.connected"),
        TunnelState::Connecting | TunnelState::Reconnecting { .. } => {
            theme.selector("state.connecting")
        }
        TunnelState::Failed { .. } => theme.selector("state.failed"),
        TunnelState::Idle => theme.selector("state.idle"),
    }
}

/// Compact route for the table: entrance → ssh host → exit.
fn route_line(spec: &TunnelSpec, theme: &Theme) -> Line<'static> {
    let arrow = Span::styled(" → ", theme.selector("route.arrow"));
    let endpoint = |text: String| Span::styled(text, theme.selector("route.endpoint"));
    let host = Span::styled(spec.host.clone(), theme.selector("route.host"));
    let target = spec
        .target
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    match spec.kind {
        TunnelKind::Local => Line::from(vec![
            endpoint(spec.listen.to_string()),
            arrow.clone(),
            host,
            arrow,
            endpoint(target),
        ]),
        TunnelKind::Remote => Line::from(vec![
            host,
            Span::styled(":", theme.selector("route.arrow")),
            endpoint(spec.listen.to_string()),
            arrow,
            Span::styled("local ", theme.selector("route.arrow")),
            endpoint(target),
        ]),
        TunnelKind::Dynamic => Line::from(vec![
            endpoint(spec.listen.to_string()),
            arrow.clone(),
            host,
            arrow,
            Span::styled("SOCKS", theme.selector("route.arrow")),
        ]),
    }
}

/// Big route diagram for the detail pane, arrows follow connection direction.
fn diagram_line(spec: &TunnelSpec, state: &TunnelState, theme: &Theme) -> Line<'static> {
    let arrow_style = if state.is_active() {
        state_style(state, theme)
    } else {
        theme.selector("route.arrow")
    };
    let arrow = Span::styled("  ══▶  ", arrow_style);
    let side = |text: &str| Span::styled(text.to_string(), theme.selector("route.arrow"));
    let endpoint = |text: String| Span::styled(text, theme.selector("route.endpoint"));
    let host = Span::styled(format!("{} (ssh)", spec.host), theme.selector("route.host"));
    let target = spec
        .target
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_default();
    match spec.kind {
        TunnelKind::Local => Line::from(vec![
            side("LOCAL "),
            endpoint(spec.listen.to_string()),
            arrow.clone(),
            host,
            arrow,
            side("REMOTE "),
            endpoint(target),
        ]),
        TunnelKind::Remote => Line::from(vec![
            side("REMOTE "),
            host,
            Span::styled(" ", arrow_style),
            endpoint(spec.listen.to_string()),
            arrow,
            side("LOCAL "),
            endpoint(target),
        ]),
        TunnelKind::Dynamic => Line::from(vec![
            side("LOCAL "),
            endpoint(spec.listen.to_string()),
            arrow.clone(),
            host,
            arrow,
            side("SOCKS proxy"),
        ]),
    }
}

fn sparkline<'a>(history: &VecDeque<u64>, width: u16, style: Style) -> Sparkline<'a> {
    let window = width as usize;
    let data: Vec<u64> = history
        .iter()
        .copied()
        .skip(history.len().saturating_sub(window))
        .collect();
    Sparkline::default().data(data).style(style)
}

fn panel_block(title: String, focused: bool, theme: &Theme) -> Block<'static> {
    let border = if focused { theme.accent } else { theme.border };
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .style(theme.style(Role::Panel))
}

fn text_input_modifiers(mut modifiers: KeyModifiers) -> bool {
    modifiers.remove(KeyModifiers::SHIFT);
    modifiers.is_empty()
}

fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    let horizontal = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1]);

    horizontal[1]
}

pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}

pub fn human_rate(bytes_per_sec: u64) -> String {
    format!("{}/s", human_bytes(bytes_per_sec))
}

pub fn fmt_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanizes_bytes() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(999), "999B");
        assert_eq!(human_bytes(1536), "1.5K");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0M");
    }

    #[test]
    fn formats_durations() {
        assert_eq!(fmt_duration(Duration::from_secs(42)), "42s");
        assert_eq!(fmt_duration(Duration::from_secs(90)), "1m30s");
        assert_eq!(fmt_duration(Duration::from_secs(3725)), "1h02m");
    }
}
