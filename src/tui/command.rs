#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Quit { force: bool },
    Help,
    History,
    Add,
    Reload,
    Theme(ThemeCommand),
    Connect(Target),
    Disconnect(Target),
    Reconnect(Target),
    Edit(Option<String>),
    Delete(Option<String>),
    Clean(Option<String>),
    Unknown(String),
    Empty,
}

/// Which tunnel a command applies to. `Selected` is the table cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Selected,
    All,
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemeCommand {
    Neutral,
    Safe,
    Danger,
}

fn target(argument: Option<&str>) -> Target {
    match argument {
        None => Target::Selected,
        Some("all") => Target::All,
        Some(name) => Target::Named(name.to_string()),
    }
}

pub fn parse(input: &str) -> Command {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed == ":" {
        return Command::Empty;
    }

    if trimmed == "\\q" {
        return Command::Quit { force: false };
    }

    let Some(command) = trimmed.strip_prefix(':') else {
        return Command::Unknown(trimmed.to_string());
    };

    let mut parts = command.split_whitespace();
    let Some(name) = parts.next() else {
        return Command::Empty;
    };
    let argument = parts.next();

    match name {
        "q" | "quit" | "exit" => Command::Quit { force: false },
        "q!" | "quit!" | "exit!" => Command::Quit { force: true },
        "help" | "?" => Command::Help,
        "history" | "hist" => Command::History,
        "add" | "new" => Command::Add,
        "reload" => Command::Reload,
        "connect" | "c" | "up" => Command::Connect(target(argument)),
        "disconnect" | "d" | "down" | "dc" => Command::Disconnect(target(argument)),
        "reconnect" | "r" => Command::Reconnect(target(argument)),
        "edit" | "e" => Command::Edit(argument.map(String::from)),
        "delete" | "del" | "rm" => Command::Delete(argument.map(String::from)),
        "clean" => Command::Clean(argument.map(String::from)),
        "theme" => match argument {
            Some("neutral") | None => Command::Theme(ThemeCommand::Neutral),
            Some("safe") | Some("green") => Command::Theme(ThemeCommand::Safe),
            Some("danger") | Some("red") | Some("unsafe") => Command::Theme(ThemeCommand::Danger),
            Some(value) => Command::Unknown(format!("unknown theme `{value}`")),
        },
        _ => Command::Unknown(command.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Command, Target, ThemeCommand, parse};

    #[test]
    fn parses_quit_aliases() {
        assert_eq!(parse(":q"), Command::Quit { force: false });
        assert_eq!(parse(":quit!"), Command::Quit { force: true });
        assert_eq!(parse("\\q"), Command::Quit { force: false });
    }

    #[test]
    fn parses_tunnel_commands() {
        assert_eq!(parse(":connect"), Command::Connect(Target::Selected));
        assert_eq!(parse(":connect all"), Command::Connect(Target::All));
        assert_eq!(
            parse(":disconnect prod-db"),
            Command::Disconnect(Target::Named("prod-db".to_string()))
        );
        assert_eq!(parse(":clean"), Command::Clean(None));
        assert_eq!(parse(":rm old"), Command::Delete(Some("old".to_string())));
    }

    #[test]
    fn parses_theme_aliases() {
        assert_eq!(parse(":theme safe"), Command::Theme(ThemeCommand::Safe));
        assert_eq!(parse(":theme unsafe"), Command::Theme(ThemeCommand::Danger));
    }
}
