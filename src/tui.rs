mod connect;
mod playback;
pub(crate) mod table;
mod training;

use std::io;

use crossterm::{
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use tokio::sync::broadcast;

use crate::{cdp, cdp::Browser, data::DataSet};

/// The outcome of running the TUI.
#[must_use]
pub enum Outcome {
    Quit,
}

/// Run the full TUI flow: browser connect, then training.
///
/// Headers are always required; there is no data preview step.
/// When `url` is provided, the browser navigates to it automatically
/// and recording starts immediately.
/// Without `url`, the connect screen prompts the user to navigate before
/// pressing Enter, after which recording starts immediately.
///
/// # Errors
///
/// Returns an error if terminal setup, drawing, or event reading fails.
pub async fn run(dataset: DataSet, url: Option<String>) -> io::Result<Outcome> {
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

    let result = run_screens(&mut terminal, dataset, url).await;

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    result
}

async fn run_screens(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    dataset: DataSet,
    url: Option<String>,
) -> io::Result<Outcome> {
    // Headers are always required — no data preview screen.
    let browser = if let Some(ref target_url) = url {
        // With --url: launch browser, navigate directly, skip connect screen.
        let browser = Browser::launch()
            .await
            .map_err(|e| io::Error::other(e.to_string()))?;

        // Subscribe before navigating so we catch the frameNavigated event.
        let mut nav_events = browser.subscribe();
        browser
            .send(
                "Page.navigate",
                Some(serde_json::json!({ "url": target_url })),
            )
            .await
            .map_err(|e| {
                io::Error::other(format!("failed to navigate to {target_url}: {e}"))
            })?;

        // Wait for the page to finish loading so the navigation event
        // doesn't leak into the training workflow as a spurious step.
        wait_for_main_frame_navigation(&mut nav_events).await?;

        browser
    } else {
        // Without --url: connect screen prompts the user to navigate,
        // then Enter triggers recording.
        match connect::run(terminal).await? {
            connect::Outcome::Continue(browser) => browser,
            connect::Outcome::Quit => return Ok(Outcome::Quit),
        }
    };

    // Recording starts immediately in both paths.
    match training::run(terminal, &dataset, browser, true).await? {
        training::TrainingOutcome::Quit => return Ok(Outcome::Quit),
        training::TrainingOutcome::ReadyForPlayback { workflow, browser } => {
            playback::run(terminal, *workflow, dataset, &browser, 1).await?;
        }
    }

    Ok(Outcome::Quit)
}

/// Wait for a main-frame `Page.frameNavigated` event.
///
/// Drains the event receiver until a main-frame navigation arrives.
/// Sub-frame navigations are ignored.
async fn wait_for_main_frame_navigation(
    events: &mut broadcast::Receiver<cdp::Event>,
) -> io::Result<()> {
    let timeout = std::time::Duration::from_secs(30);
    tokio::time::timeout(timeout, async {
        loop {
            match events.recv().await {
                Ok(event) if event.method == "Page.frameNavigated" => {
                    let is_main_frame = event
                        .params
                        .get("frame")
                        .and_then(|f| f.get("parentId"))
                        .is_none_or(serde_json::Value::is_null);
                    if is_main_frame {
                        return Ok(());
                    }
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => {
                    return Err(io::Error::other("browser connection closed during navigation"));
                }
            }
        }
    })
    .await
    .map_err(|_| io::Error::other("navigation timeout"))?
}
