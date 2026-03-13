use std::io;

use crossterm::event::{Event, EventStream, KeyCode, KeyModifiers};
use futures_util::StreamExt;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use tokio::sync::{broadcast, mpsc};

use crate::{
    cdp::{Browser, Event as CdpEvent},
    data::DataSet,
    training::{Command, TrainingCore, TrainingEvent, recorder},
    workflow::{Resolution, Step, ValueSource},
};

/// Run the training screen.
///
/// # Errors
///
/// Returns an error if drawing, event reading, or recorder setup fails.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    dataset: DataSet,
    browser: Browser,
) -> io::Result<()> {
    install_recorder(&browser)
        .await
        .map_err(|error| io::Error::other(format!("failed to install recorder: {error}")))?;

    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let mut core = TrainingCore::new(dataset.clone(), event_tx);
    let mut browser_events = browser.subscribe();
    let mut terminal_events = EventStream::new();
    let mut state = TrainingScreenState::new(&dataset);

    state.sync_from_core(&core);
    drain_training_events(&mut event_rx, &mut state, &core, &dataset);

    loop {
        terminal.draw(|frame| draw(frame, &state, &core, &dataset))?;

        tokio::select! {
            maybe_terminal_event = terminal_events.next() => {
                let Some(event_result) = maybe_terminal_event else {
                    return Ok(());
                };

                if handle_terminal_event(&event_result?, &mut core, &mut event_rx, &mut state, &dataset) {
                    return Ok(());
                }
            }
            browser_event = browser_events.recv() => {
                handle_browser_event(browser_event, &mut core, &mut event_rx, &mut state, &dataset);
            }
        }
    }
}

struct TrainingScreenState {
    step_lines: Vec<String>,
    last_status: String,
}

impl TrainingScreenState {
    fn new(dataset: &DataSet) -> Self {
        Self {
            step_lines: Vec::new(),
            last_status: initial_status(dataset),
        }
    }

    fn sync_from_core(&mut self, core: &TrainingCore) {
        self.step_lines = core.steps().iter().map(StepSummary::summary).collect();
    }
}

fn initial_status(dataset: &DataSet) -> String {
    format!(
        "Teaching row 1 of {}. Fill the form in the browser, then press Enter.",
        dataset.row_count()
    )
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

fn handle_terminal_event(
    event: &Event,
    core: &mut TrainingCore,
    event_rx: &mut mpsc::UnboundedReceiver<TrainingEvent>,
    state: &mut TrainingScreenState,
    dataset: &DataSet,
) -> bool {
    let Event::Key(key) = event else {
        return false;
    };

    if key.code == KeyCode::Char('q')
        || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    {
        return true;
    }

    if key.code == KeyCode::Enter {
        core.process(Command::AdvanceRow);
        drain_training_events(event_rx, state, core, dataset);
    }

    false
}

fn handle_browser_event(
    browser_event: Result<CdpEvent, broadcast::error::RecvError>,
    core: &mut TrainingCore,
    event_rx: &mut mpsc::UnboundedReceiver<TrainingEvent>,
    state: &mut TrainingScreenState,
    dataset: &DataSet,
) {
    match browser_event {
        Ok(event) => {
            if let Some(command) = training_command_from_cdp_event(&event) {
                core.process(command);
                drain_training_events(event_rx, state, core, dataset);
            }
        }
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            state.last_status = format!(
                "Browser events lagged; skipped {skipped} events. Continue teaching this row."
            );
        }
        Err(broadcast::error::RecvError::Closed) => {
            "Browser connection closed.".clone_into(&mut state.last_status);
        }
    }
}

fn drain_training_events(
    event_rx: &mut mpsc::UnboundedReceiver<TrainingEvent>,
    state: &mut TrainingScreenState,
    core: &TrainingCore,
    dataset: &DataSet,
) {
    while let Ok(event) = event_rx.try_recv() {
        match event {
            TrainingEvent::StepRecorded { index, step }
            | TrainingEvent::StepUpdated { index, step } => {
                update_step_line(&mut state.step_lines, index, StepSummary::summary(&step));
                state.last_status = status_for_progress(core);
            }
            TrainingEvent::ColumnBound { column, step_index } => {
                let name = column_name(dataset, column);
                state.last_status = format!(
                    "Mapped {name} to step {}. {}",
                    step_index + 1,
                    status_for_progress(core)
                );
            }
            TrainingEvent::RowComplete { row_index } => {
                state.last_status =
                    format!("Row {} complete. Press Enter to continue.", row_index + 1);
            }
            TrainingEvent::EmptyCellEncountered { column, row_index } => {
                state.last_status = format!(
                    "Row {} has an empty value for bound column {}.",
                    row_index + 1,
                    column_name(dataset, column)
                );
            }
            TrainingEvent::NewFieldEncountered { column, value } => {
                state.last_status = format!(
                    "Row {} includes an unbound value for {}: {}",
                    core.current_row_index() + 1,
                    column_name(dataset, column),
                    display_cell(&value)
                );
            }
            TrainingEvent::SpeedChanged(_) => {
                state.last_status = status_for_progress(core);
            }
            TrainingEvent::Error(message) => {
                state.last_status = message;
            }
        }
    }
}

fn update_step_line(step_lines: &mut Vec<String>, index: usize, line: String) {
    if let Some(existing) = step_lines.get_mut(index) {
        *existing = line;
        return;
    }

    if step_lines.len() < index {
        step_lines.resize(index, String::new());
    }
    step_lines.push(line);
}

fn training_command_from_cdp_event(event: &CdpEvent) -> Option<Command> {
    match event.method.as_str() {
        "Runtime.consoleAPICalled" => recorder::parse_recorder_event(&event.params),
        "Page.frameNavigated" => {
            let frame = event.params.get("frame")?;
            if frame.get("parentId").is_some() {
                return None;
            }
            let url = frame.get("url")?.as_str()?.to_owned();
            Some(Command::BrowserNavigation { url })
        }
        _ => None,
    }
}

fn draw(frame: &mut Frame, state: &TrainingScreenState, core: &TrainingCore, dataset: &DataSet) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(row_block_height(dataset)),
            Constraint::Min(8),
            Constraint::Length(4),
        ])
        .split(frame.area());

    draw_row_panel(frame, chunks[0], core, dataset);
    draw_steps_panel(frame, chunks[1], state);
    draw_status_panel(frame, chunks[2], state, core, dataset);
}

fn draw_row_panel(frame: &mut Frame, area: Rect, core: &TrainingCore, dataset: &DataSet) {
    let row_number = core.current_row_index() + 1;
    let title = format!(" Training — Row {row_number} of {} ", dataset.row_count());
    let lines = build_row_lines(core, dataset);
    let row_panel = Paragraph::new(lines)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .wrap(Wrap { trim: false });

    frame.render_widget(row_panel, area);
}

fn draw_steps_panel(frame: &mut Frame, area: Rect, state: &TrainingScreenState) {
    let items: Vec<ListItem<'_>> = if state.step_lines.is_empty() {
        vec![ListItem::new(
            Line::from("Waiting for browser actions…").dark_gray(),
        )]
    } else {
        state
            .step_lines
            .iter()
            .enumerate()
            .map(|(index, line)| ListItem::new(Line::from(format!("{}. {line}", index + 1))))
            .collect()
    };

    let steps = List::new(items).block(
        Block::default()
            .title(" Recorded steps ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Green)),
    );

    frame.render_widget(steps, area);
}

fn draw_status_panel(
    frame: &mut Frame,
    area: Rect,
    state: &TrainingScreenState,
    core: &TrainingCore,
    _dataset: &DataSet,
) {
    let summary = status_for_progress(core);
    let lines = vec![
        Line::from(summary),
        Line::from(state.last_status.as_str()),
        Line::from(vec![
            Span::styled("[Enter]", Style::default().fg(Color::Green)),
            Span::raw(" Next row   "),
            Span::styled("[q]", Style::default().fg(Color::Red)),
            Span::raw(" Quit"),
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

fn build_row_lines(core: &TrainingCore, dataset: &DataSet) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(row) = core.current_row_data() {
        for (column, value) in row.iter().enumerate() {
            lines.push(Line::from(column_line(
                dataset,
                column,
                value,
                core.bound_columns()[column],
            )));
        }
    }
    lines
}

fn column_line(dataset: &DataSet, column: usize, value: &str, bound_step: Option<usize>) -> String {
    let marker = if value.is_empty() {
        "·"
    } else if bound_step.is_some() {
        "✓"
    } else {
        "○"
    };

    let binding = match (value.is_empty(), bound_step) {
        (true, Some(step_index)) => format!("optional, mapped to step {}", step_index + 1),
        (true, None) => "optional".to_owned(),
        (false, Some(step_index)) => format!("mapped to step {}", step_index + 1),
        (false, None) => "not yet mapped".to_owned(),
    };

    format!(
        "{marker} {}: {} — {binding}",
        column_name(dataset, column),
        display_cell(value)
    )
}

fn row_block_height(dataset: &DataSet) -> u16 {
    let body_lines = u16::try_from(dataset.column_count()).unwrap_or(u16::MAX.saturating_sub(2));
    body_lines.saturating_add(2)
}

fn status_for_progress(core: &TrainingCore) -> String {
    let Some(row) = core.current_row_data() else {
        return "No active row.".to_owned();
    };

    let required = row.iter().filter(|cell| !cell.is_empty()).count();
    let bound = row
        .iter()
        .enumerate()
        .filter(|(column, cell)| !cell.is_empty() && core.bound_columns()[*column].is_some())
        .count();

    if core.is_row_complete() {
        format!("Mapped {bound}/{required} required columns. rote understands this row.")
    } else {
        format!(
            "Mapped {bound}/{required} required columns for row {}.",
            core.current_row_index() + 1
        )
    }
}

fn column_name(dataset: &DataSet, column: usize) -> String {
    dataset
        .headers()
        .and_then(|headers| headers.get(column))
        .cloned()
        .unwrap_or_else(|| format!("Column {}", column + 1))
}

fn display_cell(value: &str) -> String {
    if value.is_empty() {
        "<empty>".to_owned()
    } else {
        value.to_owned()
    }
}

struct StepSummary;

impl StepSummary {
    fn summary(step: &Step) -> String {
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
    use crate::data::{self, Delimiter};
    use crate::workflow::{Selector, Step, ValueSource};

    fn dataset() -> DataSet {
        data::from_delimited_str("name\tage\nAlice\t30\n", Delimiter::Tab, true).unwrap()
    }

    #[test]
    fn column_line_shows_bound_state() {
        let line = column_line(&dataset(), 0, "Alice", Some(1));
        assert!(line.contains('✓'));
        assert!(line.contains("mapped to step 2"));
    }

    #[test]
    fn column_line_shows_optional_empty_state() {
        let line = column_line(&dataset(), 1, "", None);
        assert!(line.contains('·'));
        assert!(line.contains("optional"));
    }

    #[test]
    fn step_summary_formats_type_steps() {
        let step = Step::Type {
            selector: Selector {
                strategies: vec![Resolution::Id {
                    id: "email".to_owned(),
                }],
                tag: "INPUT".to_owned(),
            },
            source: ValueSource::Column { index: 0 },
        };

        assert_eq!(
            StepSummary::summary(&step),
            "Type column 1 into INPUT (#email)"
        );
    }

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
