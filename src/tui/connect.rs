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
    Ready(Browser),
    Failed(String),
}

/// Run the browser connect screen.
///
/// Launches a browser and waits for the user to navigate to their form
/// and press Enter to start recording.
///
/// # Errors
///
/// Returns an error if drawing or event reading fails.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> io::Result<Outcome> {
    let mut events = EventStream::new();
    let mut state = State::Launching;
    let mut launch = launch_browser();

    loop {
        terminal.draw(|frame| draw(frame, &state))?;

        match &mut state {
            State::Launching => {
                tokio::select! {
                    browser = &mut launch => {
                        match browser {
                            Ok(browser) => {
                                state = State::Ready(browser);
                            }
                            Err(error) => {
                                state = State::Failed(error);
                            }
                        }
                    }
                    maybe_event = events.next() => {
                        let Some(event_result) = maybe_event else {
                            return Ok(Outcome::Quit);
                        };
                        if let Some(outcome) = handle_event(&event_result?, &mut state, &mut launch) {
                            return Ok(outcome);
                        }
                    }
                }
            }
            State::Ready(_) | State::Failed(_) => {
                let Some(event_result) = events.next().await else {
                    return Ok(Outcome::Quit);
                };
                if let Some(outcome) = handle_event(&event_result?, &mut state, &mut launch) {
                    return Ok(outcome);
                }
            }
        }
    }
}

type LaunchFuture = Pin<Box<dyn futures_util::Future<Output = Result<Browser, String>>>>;

fn launch_browser() -> LaunchFuture {
    Box::pin(async { Browser::launch().await.map_err(|error| error.to_string()) })
}

fn handle_event(event: &Event, state: &mut State, launch: &mut LaunchFuture) -> Option<Outcome> {
    let Event::Key(key) = event else {
        return None;
    };

    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return Some(Outcome::Quit);
    }

    match state {
        State::Ready(_) if key.code == KeyCode::Enter => {
            let ready_state = std::mem::replace(state, State::Launching);
            if let State::Ready(browser) = ready_state {
                return Some(Outcome::Continue(browser));
            }
        }
        State::Failed(_) if key.code == KeyCode::Enter || key.code == KeyCode::Char('r') => {
            *state = State::Launching;
            *launch = launch_browser();
        }
        State::Launching | State::Ready(_) | State::Failed(_) => {}
    }

    None
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
        State::Ready(_) => vec![
            Line::from("Browser launched."),
            Line::from(""),
            Line::from("Navigate to your form and press Enter to start recording."),
            Line::from(vec![
                Span::styled("[Enter]", Style::default().fg(Color::Green)),
                Span::raw(" Start recording   "),
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
