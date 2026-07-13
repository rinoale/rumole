pub mod command;
pub mod menu;
pub mod theme;

use std::{io::stdout, time::Duration};

use color_eyre::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind},
    execute,
};
use ratatui::DefaultTerminal;
use rustui::runtime::spawn_event_reader;
use tokio::time;

use crate::app::App;

pub async fn run(mut app: App) -> Result<()> {
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture)?;

    let result = run_app(&mut terminal, &mut app).await;

    // Kill ssh children and unlink owned sockets before leaving the terminal.
    app.shutdown().await;

    let disable_result = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();
    disable_result?;
    result
}

async fn run_app(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    let mut events = spawn_event_reader();
    let mut tick = time::interval(Duration::from_millis(50));

    while !app.should_quit() {
        app.on_tick();
        terminal.draw(|frame| app.render(frame))?;

        tokio::select! {
            maybe_event = events.recv() => {
                match maybe_event {
                    Some(Event::Key(key)) if key.kind == KeyEventKind::Press => {
                        app.handle_key_event(key);
                    }
                    Some(Event::Mouse(mouse)) => app.handle_mouse_event(mouse),
                    Some(_) => {}
                    None => break,
                }
            }
            _ = tick.tick() => {}
        }
    }

    Ok(())
}
