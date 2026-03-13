use std::{io, pin::Pin};

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use crate::cdp::Browser;

/// The outcome of running the connect screen.
#[must_use]
pub enum Outcome {
    Continue(Browser),
    Quit,
}

enum State {
    Launching,
    Failed(String),
}

/// Run the browser connect screen.
///
/// # Errors
///
/// Returns an error if drawing or event reading fails.
pub async fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<Outcome> {
    let mut events = EventStream::new();
    let mut state = State::Launching;
    let mut launch = launch_browser();

    loop {
        terminal.draw(|frame| draw(frame, &state))?;

        if matches!(state, State::Launching) {
            tokio::select! {
                browser = &mut launch => {
                    match browser {
                        Ok(browser) => return Ok(Outcome::Continue(browser)),
                        Err(error) => {
                            state = State::Failed(error);
                        }
                    }
                }
                maybe_event = events.next() => {
                    let Some(event_result) = maybe_event else {
                        return Ok(Outcome::Quit);
                    };
                    if handle_event(&event_result?, &mut state, &mut launch) {
                        return Ok(Outcome::Quit);
                    }
                }
            }
        } else {
            let Some(event_result) = events.next().await else {
                return Ok(Outcome::Quit);
            };
            if handle_event(&event_result?, &mut state, &mut launch) {
                return Ok(Outcome::Quit);
            }
        }
    }
}

type LaunchFuture = Pin<Box<dyn futures_util::Future<Output = Result<Browser, String>>>>;

fn launch_browser() -> LaunchFuture {
    Box::pin(async { Browser::launch().await.map_err(|error| error.to_string()) })
}

fn handle_event(event: &Event, state: &mut State, launch: &mut LaunchFuture) -> bool {
    let Event::Key(key) = event else {
        return false;
    };

    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return true;
    }

    if matches!(state, State::Failed(_))
        && (key.code == KeyCode::Enter || key.code == KeyCode::Char('r'))
    {
        *state = State::Launching;
        *launch = launch_browser();
    }

    false
}

fn draw(frame: &mut Frame, state: &State) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Length(8),
            Constraint::Percentage(40),
        ])
        .split(frame.area());

    let lines = match state {
        State::Launching => vec![
            Line::from("Launching browser and connecting to Chrome DevTools…"),
            Line::from(""),
            Line::from("A Chrome or Edge window should appear shortly."),
            Line::from(vec![
                Span::styled("[q]", Style::default().fg(Color::Red)),
                Span::raw(" Quit"),
            ]),
        ],
        State::Failed(error) => vec![
            Line::from("Failed to connect to the browser."),
            Line::from(""),
            Line::from(error.as_str()).red(),
            Line::from(vec![
                Span::styled("[Enter]", Style::default().fg(Color::Green)),
                Span::raw(" Retry   "),
                Span::styled("[q]", Style::default().fg(Color::Red)),
                Span::raw(" Quit"),
            ]),
        ],
    };

    let paragraph = Paragraph::new(lines).block(
        Block::default()
            .title(" Connect Browser ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    frame.render_widget(paragraph, chunks[1]);
}
