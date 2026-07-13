#![allow(dead_code)]

use ratatui::style::{Color, Style};

use rustui::style::{ColorToken, Design, Palette, Role, style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeKind {
    Neutral,
    Safe,
    Danger,
}

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: &'static str,
    pub background: Color,
    pub panel: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub accent: Color,
    pub accent_dark: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub design: Design,
}

impl Theme {
    pub fn style(&self, role: Role) -> Style {
        self.design.role_style(role)
    }

    pub fn selector(&self, selector: &str) -> Style {
        self.design.style(selector)
    }

    pub fn color(&self, token: ColorToken) -> Color {
        self.design.color(token)
    }

    pub fn palette(&self) -> Palette {
        self.design.palette
    }
}

impl ThemeKind {
    pub fn theme(self) -> Theme {
        let (name, palette) = match self {
            Self::Neutral => (
                "neutral",
                Palette {
                    surface0: Color::Rgb(14, 17, 22),
                    surface1: Color::Rgb(23, 28, 36),
                    surface2: Color::Rgb(33, 40, 52),
                    border: Color::Rgb(85, 95, 109),
                    text: Color::Rgb(232, 236, 241),
                    muted: Color::Rgb(151, 161, 175),
                    accent: Color::Rgb(89, 165, 216),
                    accent_low: Color::Rgb(34, 80, 111),
                    success: Color::Rgb(81, 178, 118),
                    warning: Color::Rgb(226, 173, 83),
                    danger: Color::Rgb(222, 96, 92),
                    selection: Color::Rgb(38, 72, 96),
                },
            ),
            Self::Safe => (
                "safe",
                Palette {
                    surface0: Color::Rgb(11, 20, 16),
                    surface1: Color::Rgb(20, 35, 27),
                    surface2: Color::Rgb(28, 48, 36),
                    border: Color::Rgb(70, 132, 98),
                    text: Color::Rgb(232, 241, 235),
                    muted: Color::Rgb(148, 171, 156),
                    accent: Color::Rgb(75, 190, 121),
                    accent_low: Color::Rgb(28, 92, 56),
                    success: Color::Rgb(75, 190, 121),
                    warning: Color::Rgb(226, 173, 83),
                    danger: Color::Rgb(222, 96, 92),
                    selection: Color::Rgb(31, 83, 54),
                },
            ),
            Self::Danger => (
                "danger",
                Palette {
                    surface0: Color::Rgb(24, 13, 13),
                    surface1: Color::Rgb(39, 22, 22),
                    surface2: Color::Rgb(55, 31, 31),
                    border: Color::Rgb(143, 76, 72),
                    text: Color::Rgb(246, 232, 230),
                    muted: Color::Rgb(182, 145, 140),
                    accent: Color::Rgb(228, 93, 86),
                    accent_low: Color::Rgb(111, 36, 34),
                    success: Color::Rgb(81, 178, 118),
                    warning: Color::Rgb(226, 173, 83),
                    danger: Color::Rgb(228, 93, 86),
                    selection: Color::Rgb(96, 39, 37),
                },
            ),
        };

        Theme {
            name,
            background: palette.surface0,
            panel: palette.surface1,
            border: palette.border,
            text: palette.text,
            muted: palette.muted,
            accent: palette.accent,
            accent_dark: palette.accent_low,
            success: palette.success,
            warning: palette.warning,
            danger: palette.danger,
            design: base_design(palette),
        }
    }
}

fn base_design(palette: Palette) -> Design {
    Design::new(palette)
        .role(
            Role::AppBackground,
            style().fg(palette.text).bg(palette.surface0),
        )
        .role(Role::Header, style().fg(palette.text).bg(palette.surface1))
        .role(
            Role::HeaderBrand,
            style().fg(palette.surface0).bg(palette.accent).bold(),
        )
        .role(
            Role::HeaderMode,
            style().fg(palette.text).bg(palette.accent_low).bold(),
        )
        .role(Role::Panel, style().fg(palette.text).bg(palette.surface1))
        .role(
            Role::PanelFocused,
            style().fg(palette.accent).bg(palette.surface1),
        )
        .role(Role::PanelTitle, style().fg(palette.accent).bold())
        .role(Role::Text, style().fg(palette.text).bg(palette.surface1))
        .role(Role::TextMuted, style().fg(palette.muted).bg(palette.surface1))
        .role(Role::ListItem, style().fg(palette.text).bg(palette.surface1))
        .role(
            Role::ListItemSelected,
            style().fg(palette.text).bg(palette.selection).bold(),
        )
        .role(Role::Command, style().fg(palette.text).bg(palette.surface1))
        .role(
            Role::CommandFocused,
            style().fg(palette.accent).bg(palette.surface1),
        )
        .role(Role::PromptPrefix, style().fg(palette.accent).bold())
        .role(Role::PromptInput, style().fg(palette.text))
        .role(Role::Footer, style().fg(palette.text).bg(palette.surface1))
        .role(Role::FooterKey, style().fg(palette.accent).bold())
        .role(Role::HelpOverlay, style().fg(palette.text).bg(palette.surface2))
        .role(Role::HelpTitle, style().fg(palette.accent).bold())
        .role(Role::Status, style().fg(palette.muted))
        .role(Role::StatusSuccess, style().fg(palette.success).bold())
        .role(Role::StatusWarning, style().fg(palette.warning).bold())
        .role(Role::StatusDanger, style().fg(palette.danger).bold())
        .role(Role::Button, style().fg(palette.text).bg(palette.surface2))
        .role(
            Role::ButtonPrimary,
            style().fg(palette.surface0).bg(palette.accent).bold(),
        )
        .role(
            Role::ButtonDanger,
            style().fg(palette.surface0).bg(palette.danger).bold(),
        )
        .role(Role::ButtonFocused, style().fg(palette.accent).underlined())
        .role(Role::Input, style().fg(palette.text).bg(palette.surface2))
        .role(
            Role::InputFocused,
            style().fg(palette.text).bg(palette.surface2).underlined(),
        )
        .role(Role::Badge, style().fg(palette.surface0).bg(palette.accent_low))
        .role(
            Role::BadgeSuccess,
            style().fg(palette.surface0).bg(palette.success).bold(),
        )
        .role(
            Role::BadgeWarning,
            style().fg(palette.surface0).bg(palette.warning).bold(),
        )
        .role(
            Role::BadgeDanger,
            style().fg(palette.surface0).bg(palette.danger).bold(),
        )
        .role(
            Role::TableHeader,
            style().fg(palette.surface0).bg(palette.accent).bold(),
        )
        .role(Role::TableCell, style().fg(palette.text).bg(palette.surface1))
        .role(
            Role::TableCellSelected,
            style().fg(palette.text).bg(palette.selection),
        )
        .selector("traffic.up", style().fg(palette.warning).bold())
        .selector("traffic.down", style().fg(palette.success).bold())
        .selector("route.endpoint", style().fg(palette.text).bold())
        .selector("route.host", style().fg(palette.accent).bold())
        .selector("route.arrow", style().fg(palette.muted))
        .selector("state.connected", style().fg(palette.success).bold())
        .selector("state.connecting", style().fg(palette.warning).bold())
        .selector("state.failed", style().fg(palette.danger).bold())
        .selector("state.idle", style().fg(palette.muted))
}
