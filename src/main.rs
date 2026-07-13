mod app;
mod config;
mod history;
mod manager;
mod relay;
mod tui;
mod tunnel;

use color_eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    tui::run(app::App::new()).await
}
