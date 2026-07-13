use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use rustui::keymap::Keymap;
use rustui::style::Role;

use super::theme::Theme;
use crate::app::Intent;

const FOOTER_ORDER: [Intent; 7] = [
    Intent::Connect,
    Intent::Disconnect,
    Intent::Reconnect,
    Intent::Add,
    Intent::Edit,
    Intent::History,
    Intent::Help,
];

pub fn footer(keymap: &Keymap<Intent>, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for intent in FOOTER_ORDER {
        if let Some(binding) = keymap.bindings().iter().find(|b| b.intent == intent) {
            spans.push(Span::styled(
                binding.label,
                theme.style(Role::FooterKey).add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::raw(format!(" {}  ", binding.description)));
        }
    }
    spans.push(Span::styled(
        ":q",
        theme.style(Role::FooterKey).add_modifier(Modifier::BOLD),
    ));
    spans.push(Span::raw(" quit"));
    Line::from(spans)
}

pub fn help_lines(keymap: &Keymap<Intent>) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from("rumole — ssh tunnel manager"),
        Line::from(""),
        Line::from("Quit is command-mode only: :q, :quit, :exit, :q!"),
        Line::from("Esc cancels transient state; it never quits."),
        Line::from(""),
        Line::from("Keys"),
    ];

    for binding in keymap.bindings() {
        lines.push(Line::from(format!(
            "  {:<10} {}",
            binding.label, binding.description
        )));
    }

    lines.extend([
        Line::from(""),
        Line::from("Commands"),
        Line::from("  :connect [name|all]      connect tunnel(s)"),
        Line::from("  :disconnect [name|all]   disconnect tunnel(s)"),
        Line::from("  :reconnect [name]        drop and re-establish"),
        Line::from("  :add                     new tunnel form"),
        Line::from("  :edit [name]             edit tunnel (while disconnected)"),
        Line::from("  :delete [name]           delete tunnel"),
        Line::from("  :clean [name]            remove leftover unix socket file"),
        Line::from("                           (runs `rm -f` over ssh for -R sockets)"),
        Line::from("  :reload                  re-read tunnels.toml"),
        Line::from("  :history                 event history"),
        Line::from("  :theme neutral|safe|danger"),
        Line::from("  :q, :quit, :exit, :q!    quit"),
        Line::from(""),
        Line::from("Unix sockets"),
        Line::from("  ssh runs with StreamLocalBindUnlink=yes, so stale socket"),
        Line::from("  files are replaced automatically on reconnect. Sockets on"),
        Line::from("  this machine are also unlinked when a tunnel stops; use"),
        Line::from("  :clean for anything left behind (e.g. after a crash)."),
    ]);

    lines
}
