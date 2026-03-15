use std::io;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use tokio::sync::{broadcast, mpsc};

use super::table::{self, CellState, RowState, TableState};
use crate::{
    cdp::{Browser, Event as CdpEvent},
    data::DataSet,
    training::{Command, TrainingCore, TrainingEvent, recorder},
    workflow::{Resolution, Step, ValueSource, Workflow},
};

/// Result of the training screen.
pub enum TrainingOutcome {
    /// User quit without completing training.
    Quit,
    /// Row 1 trained; ready for cell-by-cell playback of remaining rows.
    ReadyForPlayback {
        workflow: Box<Workflow>,
        browser: Browser,
    },
}

/// Run the training screen.
///
/// Trains exactly row 1, then returns `ReadyForPlayback` when the user
/// presses Enter after completing that row.
///
/// When `start_recording` is `true`, recording begins immediately without
/// waiting for the user to press Enter.
/// When `false`, the screen waits for Enter before capturing browser events.
///
/// # Errors
///
/// Returns an error if the training row contains empty cells, or if
/// drawing, event reading, or recorder setup fails.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    dataset: &DataSet,
    browser: Browser,
    start_recording: bool,
) -> io::Result<TrainingOutcome> {
    validate_training_row(dataset, 0)?;

    // Fresh subscription — created after any prior navigation so no buffered
    // events from browser setup are included.
    let mut browser_events = browser.subscribe();

    install_recorder(&browser)
        .await
        .map_err(|error| io::Error::other(format!("failed to install recorder: {error}")))?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut core = TrainingCore::new(dataset.clone(), event_tx);
    let mut terminal_events = EventStream::new();

    let mut table_state = TableState::new(dataset.row_count(), dataset.column_count(), dataset);
    table_state.set_row_state(0, RowState::InProgress);

    let mut recording = start_recording;
    let mut capturing_transition = false;
    let mut last_status = if start_recording {
        format!(
            "Teaching row 1 of {}. Fill the form in the browser.",
            dataset.row_count()
        )
    } else {
        "Navigate to your form and press Enter to start recording.".to_owned()
    };

    loop {
        terminal.draw(|frame| {
            let table_area_height = frame.area().height.saturating_sub(5);
            table_state.update_viewport(table_area_height);
            draw(
                frame,
                &table_state,
                &last_status,
                recording,
                capturing_transition,
                &core,
                dataset,
            );
        })?;

        tokio::select! {
            maybe_terminal_event = terminal_events.next() => {
                let Some(event_result) = maybe_terminal_event else {
                    return Ok(TrainingOutcome::Quit);
                };

                match handle_terminal_event(
                    &event_result?,
                    &core,
                    &mut table_state,
                    recording,
                ) {
                    TerminalAction::Continue => {}
                    TerminalAction::Quit => return Ok(TrainingOutcome::Quit),
                    TerminalAction::StartRecording => {
                        recording = true;
                        last_status = format!(
                            "Teaching row 1 of {}. Fill the form in the browser.",
                            dataset.row_count()
                        );
                    }
                    TerminalAction::Done => {
                        let workflow = Box::new(core.build_workflow(None));
                        return Ok(TrainingOutcome::ReadyForPlayback { workflow, browser });
                    }
                }
            }
            browser_event = browser_events.recv(), if recording => {
                handle_browser_event(
                    browser_event,
                    &mut core,
                    &mut event_rx,
                    &mut table_state,
                    &mut last_status,
                    &mut capturing_transition,
                    dataset,
                );
            }
        }
    }
}

async fn install_recorder(browser: &Browser) -> Result<(), crate::cdp::CdpError> {
    browser
        .send(
            "Page.addScriptToEvaluateOnNewDocument",
            Some(recorder::auto_inject_params()),
        )
        .await?;
    browser
        .evaluate(recorder::RECORDER_SCRIPT)
        .await
        .map(|_| ())
}

enum TerminalAction {
    Continue,
    Quit,
    /// Enter pressed in the pre-recording gate — begin capturing.
    StartRecording,
    Done,
}

fn handle_terminal_event(
    event: &Event,
    core: &TrainingCore,
    table_state: &mut TableState,
    recording: bool,
) -> TerminalAction {
    let Event::Key(key) = event else {
        return TerminalAction::Continue;
    };

    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return TerminalAction::Quit;
    }

    if key.code == KeyCode::Enter {
        if !recording {
            return TerminalAction::StartRecording;
        }

        if core.is_row_complete() {
            // Training covers exactly row 1.
            // Mark it done and transition to cell-by-cell playback.
            let current = core.current_row_index();
            table_state.set_row_state(current, RowState::Done);
            return TerminalAction::Done;
        }
    }

    TerminalAction::Continue
}

fn handle_browser_event(
    browser_event: Result<CdpEvent, broadcast::error::RecvError>,
    core: &mut TrainingCore,
    event_rx: &mut mpsc::UnboundedReceiver<TrainingEvent>,
    table_state: &mut TableState,
    last_status: &mut String,
    capturing_transition: &mut bool,
    dataset: &DataSet,
) {
    match browser_event {
        Ok(event) => {
            if let Some(command) = training_command_from_cdp_event(&event) {
                core.process(command);
                drain_training_events(
                    event_rx,
                    table_state,
                    last_status,
                    capturing_transition,
                    core,
                    dataset,
                );
            }
        }
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            *last_status = format!(
                "Browser events lagged; skipped {skipped} events. Continue teaching this row."
            );
        }
        Err(broadcast::error::RecvError::Closed) => {
            // Clippy pedantic wants clone_into here for allocation reuse,
            // but readability wins on an error path that runs at most once.
            #[allow(clippy::assigning_clones)]
            {
                *last_status = "Browser connection closed.".to_owned();
            }
        }
    }
}

fn drain_training_events(
    event_rx: &mut mpsc::UnboundedReceiver<TrainingEvent>,
    table_state: &mut TableState,
    last_status: &mut String,
    capturing_transition: &mut bool,
    core: &TrainingCore,
    dataset: &DataSet,
) {
    while let Ok(event) = event_rx.try_recv() {
        match event {
            TrainingEvent::StepRecorded { index, step } => {
                if *capturing_transition {
                    *last_status = format!(
                        "Captured end-of-row step {}: {}",
                        index + 1,
                        step_summary(&step),
                    );
                } else {
                    *last_status = format!("Step {} recorded: {}", index + 1, step_summary(&step));
                }
            }
            TrainingEvent::StepUpdated { .. } | TrainingEvent::SpeedChanged(_) => {}
            TrainingEvent::ColumnBound { column, step_index } => {
                let name = column_name(dataset, column);
                *last_status = format!("Mapped {name} to step {}.", step_index + 1);
                table_state.set_cell_state(core.current_row_index(), column, CellState::Done);
            }
            TrainingEvent::RowComplete { row_index: _ } => {
                *capturing_transition = true;
                // Clippy pedantic wants clone_into here for allocation reuse,
                // but readability wins on this infrequent path.
                #[allow(clippy::assigning_clones)]
                {
                    *last_status =
                        "All fields mapped. Submit the form and navigate back, then press Enter."
                            .to_owned();
                }
            }
            TrainingEvent::RowAdvanced { row_index } => {
                // Previous row is done; the new row is now in progress.
                table_state.set_row_state(row_index.saturating_sub(1), RowState::Done);
                table_state.set_row_state(row_index, RowState::InProgress);
                // Viewport is updated before each draw with the real terminal height.
            }
            TrainingEvent::EmptyCellEncountered { column, row_index } => {
                *last_status = format!(
                    "Row {} has an empty value for bound column {}.",
                    row_index + 1,
                    column_name(dataset, column)
                );
            }
            TrainingEvent::NewFieldEncountered { column, value } => {
                *last_status = format!(
                    "Row {} includes an unbound value for {}: {}",
                    core.current_row_index() + 1,
                    column_name(dataset, column),
                    if value.is_empty() { "<empty>" } else { &value }
                );
            }
            TrainingEvent::Error(message) => {
                *last_status = message;
            }
        }
    }
}

fn training_command_from_cdp_event(event: &CdpEvent) -> Option<Command> {
    match event.method.as_str() {
        "Runtime.consoleAPICalled" => recorder::parse_recorder_event(&event.params),
        "Page.frameNavigated" => {
            let frame = event.params.get("frame")?;
            let is_sub_frame = frame.get("parentId").is_some_and(|v| !v.is_null());
            if is_sub_frame {
                return None;
            }
            let url = frame.get("url")?.as_str()?.to_owned();
            Some(Command::BrowserNavigation { url })
        }
        _ => None,
    }
}

fn draw(
    frame: &mut Frame,
    table_state: &TableState,
    last_status: &str,
    recording: bool,
    capturing_transition: bool,
    core: &TrainingCore,
    dataset: &DataSet,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(5)])
        .split(frame.area());

    let table_area = chunks[0];
    let status_area = chunks[1];

    table::draw_table(frame, table_area, dataset, table_state);
    draw_status_bar(
        frame,
        status_area,
        last_status,
        recording,
        capturing_transition,
        core,
        dataset,
    );
}

fn draw_status_bar(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    last_status: &str,
    recording: bool,
    capturing_transition: bool,
    core: &TrainingCore,
    dataset: &DataSet,
) {
    let current = core.current_row_index();
    let total = dataset.row_count();

    let bound = core.bound_columns().iter().filter(|b| b.is_some()).count();
    let required = core
        .current_row_data()
        .map_or(0, |r| r.iter().filter(|c| !c.is_empty()).count());

    let progress = if !recording {
        format!(
            "Training — Row {} of {}   Waiting to start",
            current + 1,
            total
        )
    } else if capturing_transition {
        format!(
            "Training — Row {} of {}   Mapped {}/{} columns — recording end-of-row actions",
            current + 1,
            total,
            bound,
            required,
        )
    } else {
        format!(
            "Training — Row {} of {}   Mapped {}/{} columns",
            current + 1,
            total,
            bound,
            required,
        )
    };

    let enter_hint = if !recording {
        "Start recording"
    } else if capturing_transition {
        "Form is ready — start playback"
    } else {
        "(map all fields first)"
    };

    let lines = vec![
        Line::from(progress),
        Line::from(last_status),
        Line::from(vec![
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Quit   "),
            Span::styled("[Enter]", Style::default().fg(Color::Green)),
            Span::raw(format!(" {enter_hint}")),
        ]),
    ];

    let status = Paragraph::new(lines).block(
        Block::default()
            .title(" Status ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow)),
    );

    frame.render_widget(status, area);
}

/// Verify that the training row has no empty cells.
///
/// Column-order training requires every cell in the training row to have a
/// value so that left-to-right binding works without gaps.
fn validate_training_row(dataset: &DataSet, row_index: usize) -> io::Result<()> {
    let Some(row) = dataset.row(row_index) else {
        return Err(io::Error::other("training row is out of range"));
    };

    let empty_columns: Vec<String> = row
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.is_empty())
        .map(|(col, _)| column_name(dataset, col))
        .collect();

    if empty_columns.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "Training row has empty cells in: {}. \
             Every column must have a value in the training row.",
            empty_columns.join(", "),
        )))
    }
}

fn column_name(dataset: &DataSet, column: usize) -> String {
    dataset
        .headers()
        .and_then(|headers| headers.get(column))
        .cloned()
        .unwrap_or_else(|| format!("Column {}", column + 1))
}

/// Summarize a step for status messages.
fn step_summary(step: &Step) -> String {
    match step {
        Step::Click { selector } => {
            format!("Click {}", describe_selector(selector))
        }
        Step::Type { selector, source } => {
            format!(
                "Type {} into {}",
                describe_source(source),
                describe_selector(selector)
            )
        }
        Step::WaitForNavigation => "Wait for navigation".to_owned(),
    }
}

fn describe_source(source: &ValueSource) -> String {
    match source {
        ValueSource::Column { index } => format!("column {}", index + 1),
        ValueSource::Literal { value } => format!("literal \"{value}\""),
    }
}

fn describe_selector(selector: &crate::workflow::Selector) -> String {
    let target = selector
        .strategies
        .iter()
        .map(|strategy| match strategy {
            Resolution::Id { id } => format!("#{id}"),
            Resolution::Css { selector } => selector.clone(),
            Resolution::XPath { path } => path.clone(),
            Resolution::TextContent { text } => format!("text \"{text}\""),
        })
        .next()
        .unwrap_or_else(|| selector.tag.clone());

    format!("{} ({target})", selector.tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cdp::Event as CdpEvent;

    #[test]
    fn training_command_maps_top_level_navigation() {
        let event = CdpEvent {
            method: "Page.frameNavigated".to_owned(),
            params: serde_json::json!({
                "frame": {
                    "url": "https://example.com/form"
                }
            }),
        };

        let command = training_command_from_cdp_event(&event).unwrap();
        match command {
            Command::BrowserNavigation { url } => {
                assert_eq!(url, "https://example.com/form");
            }
            _ => panic!("expected BrowserNavigation"),
        }
    }
}
