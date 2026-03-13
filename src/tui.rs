mod connect;
mod data;
mod playback;
mod training;

use std::io;

use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::data::DataSet;

/// The outcome of running the TUI.
#[must_use]
pub enum Outcome {
    Quit,
}

/// Run the full TUI flow: data preview, browser connect, then training.
///
/// # Errors
///
/// Returns an error if terminal setup, drawing, or event reading fails.
pub async fn run(dataset: DataSet) -> io::Result<Outcome> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    let result = run_screens(&mut terminal, dataset).await;

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    result
}

async fn run_screens(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    dataset: DataSet,
) -> io::Result<Outcome> {
    let dataset = match data::run(terminal, dataset).await? {
        data::Outcome::Continue(dataset) => dataset,
        data::Outcome::Quit => return Ok(Outcome::Quit),
    };

    let browser = match connect::run(terminal).await? {
        connect::Outcome::Continue(browser) => browser,
        connect::Outcome::Quit => return Ok(Outcome::Quit),
    };

    match training::run(terminal, dataset.clone(), browser).await? {
        training::TrainingOutcome::Quit => return Ok(Outcome::Quit),
        training::TrainingOutcome::ReadyForPlayback { workflow, browser } => {
            let _outcome = playback::run(terminal, *workflow, dataset, &browser, 1).await?;
        }
    }
    Ok(Outcome::Quit)
}
