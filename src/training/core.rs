// Training core state machine implementation.

use std::collections::{BTreeMap, HashSet};
use std::mem;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::data::{DataSet, DataSourceConfig};
use crate::workflow::{
    EmptyCellRule, NavKey, NavigationPath, PlaybackSpeed, Selector, Step, ValueSource, Workflow,
};

use super::{Command, SelectorInfo, TrainingEvent};

/// The training session state machine.
///
/// Processes browser events and user commands to build a workflow template.
/// Emits `TrainingEvent`s for the TUI layer to render.
pub struct TrainingCore {
    data: DataSet,
    current_row: usize,
    steps: Vec<Step>,
    /// Tracks the `SelectorInfo` that produced each step, for same-element matching.
    step_selectors: Vec<Option<SelectorInfo>>,
    /// Inter-step delays captured during training, parallel to `steps`.
    ///
    /// `step_delays[i]` is the elapsed time before step `i` was recorded
    /// (zero for the first step).
    step_delays: Vec<Duration>,
    /// Instant when the most recent step was recorded.
    ///
    /// `None` before any step has been recorded.
    last_step_time: Option<Instant>,
    /// Captured when all fields are mapped (`RowComplete`).
    ///
    /// Measures the gap from the last recorded step to the moment training
    /// considers the row done — before the user reviews the TUI or presses
    /// Enter.
    row_end_delay: Duration,
    /// `column_bindings[col]` = the step index that column is bound to.
    column_bindings: Vec<Option<usize>>,
    empty_cell_rules: BTreeMap<usize, EmptyCellRule>,
    speed: PlaybackSpeed,
    event_tx: mpsc::UnboundedSender<TrainingEvent>,
    /// Selector of the last clicked element — starting point for tab navigation.
    ///
    /// `None` when no click has been recorded yet (tabs navigate from the document).
    pending_anchor: Option<Selector>,
    /// Tab key presses accumulated since the last click (or session start).
    ///
    /// Non-empty when the user has tabbed to reach the current element.
    /// Consumed (attached to the next step and cleared) when an input or click is recorded.
    pending_keys: Vec<NavKey>,
}

impl TrainingCore {
    /// Create a new training session with the given data set.
    pub fn new(data: DataSet, event_tx: mpsc::UnboundedSender<TrainingEvent>) -> Self {
        let col_count = data.column_count();
        Self {
            data,
            current_row: 0,
            steps: Vec::new(),
            step_selectors: Vec::new(),
            step_delays: Vec::new(),
            last_step_time: None,
            row_end_delay: Duration::ZERO,
            column_bindings: vec![None; col_count],
            empty_cell_rules: BTreeMap::new(),
            speed: PlaybackSpeed::default(),
            event_tx,
            pending_anchor: None,
            pending_keys: Vec::new(),
        }
    }

    /// Process a command and emit any resulting events.
    ///
    /// This method is synchronous because training sessions are driven by a
    /// single-threaded event loop in the TUI. All browser events and user
    /// commands are serialised through this entry point, so there is no risk
    /// of concurrent mutation.
    pub fn process(&mut self, command: Command) {
        match command {
            Command::BrowserClick { selector_info, tag } => {
                self.handle_click(selector_info, tag);
            }
            Command::BrowserInput {
                selector_info,
                tag,
                value,
            } => {
                self.handle_input(selector_info, tag, &value);
            }
            Command::BrowserNavigation { url } => {
                self.handle_navigation(url);
            }
            Command::BrowserTab { shift } => {
                self.handle_tab(shift);
            }
            Command::AdvanceRow => {
                self.handle_advance_row();
            }
            Command::SetSpeed(speed) => {
                self.speed = speed;
                self.emit(TrainingEvent::SpeedChanged(speed));
            }
            Command::HandleEmptyCell { column, rule } => {
                self.empty_cell_rules.insert(column, rule);
            }
            Command::HandleNewField { column } => {
                self.handle_new_field(column);
            }
        }
    }

    // -- Accessors --

    /// Index of the current data row.
    pub fn current_row_index(&self) -> usize {
        self.current_row
    }

    /// Data values for the current row.
    pub fn current_row_data(&self) -> Option<&[String]> {
        self.data.row(self.current_row)
    }

    /// The recorded steps so far.
    #[cfg(test)]
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    /// Current playback speed.
    #[cfg(test)]
    pub fn speed(&self) -> PlaybackSpeed {
        self.speed
    }

    /// Column binding state: `bound_columns()[col]` is `Some(step_index)` if bound.
    pub fn bound_columns(&self) -> &[Option<usize>] {
        &self.column_bindings
    }

    /// The current per-column empty-cell rules.
    #[cfg(test)]
    pub fn empty_cell_rules(&self) -> &BTreeMap<usize, EmptyCellRule> {
        &self.empty_cell_rules
    }

    /// Whether every non-empty column in the current row is bound to a step.
    pub fn is_row_complete(&self) -> bool {
        let Some(row) = self.data.row(self.current_row) else {
            return false;
        };
        for (col, cell) in row.iter().enumerate() {
            if !cell.is_empty() && self.column_bindings.get(col).copied().flatten().is_none() {
                return false;
            }
        }
        true
    }

    /// Build a [`Workflow`] from the current training state.
    ///
    /// The optional `data_source` is embedded in the workflow so that playback
    /// can reload the data automatically.
    pub fn build_workflow(&self, data_source: Option<DataSourceConfig>) -> Workflow {
        Workflow::new(
            self.data.column_count(),
            self.steps.clone(),
            self.step_delays.clone(),
            self.row_end_delay,
            self.column_bindings.clone(),
            self.empty_cell_rules.clone(),
            data_source,
        )
    }

    // -- Private handlers --

    /// Compute the delay since the last recorded step and update `last_step_time`.
    ///
    /// Returns `Duration::ZERO` for the first step (no previous reference point).
    fn record_step_time(&mut self) -> Duration {
        let now = Instant::now();
        let delay = self
            .last_step_time
            .map_or(Duration::ZERO, |t| now.duration_since(t));
        self.last_step_time = Some(now);
        delay
    }

    fn handle_click(&mut self, selector_info: SelectorInfo, tag: String) {
        let delay = self.record_step_time();

        // If the user tabbed to this element, attach the navigation path.
        let navigation = if self.pending_keys.is_empty() {
            None
        } else {
            Some(NavigationPath {
                anchor: self.pending_anchor.take(),
                keys: mem::take(&mut self.pending_keys),
            })
        };

        let selector = selector_info.clone().into_selector(tag);

        // This click becomes the new navigation anchor.
        self.pending_anchor = Some(selector.clone());

        let step = Step::Click {
            selector,
            navigation,
        };
        let index = self.steps.len();
        self.steps.push(step.clone());
        self.step_selectors.push(Some(selector_info));
        self.step_delays.push(delay);
        self.emit(TrainingEvent::StepRecorded { index, step });
    }

    /// Maximum number of tab keys that can accumulate before being attached to a step.
    ///
    /// A real user won't tab more than ~20 times to reach a field. A higher limit
    /// guards against synthetic events without being restrictive.
    const MAX_PENDING_KEYS: usize = 64;

    fn handle_tab(&mut self, shift: bool) {
        if self.pending_keys.len() >= Self::MAX_PENDING_KEYS {
            return;
        }
        let key = if shift { NavKey::ShiftTab } else { NavKey::Tab };
        self.pending_keys.push(key);
    }

    fn handle_input(&mut self, selector_info: SelectorInfo, tag: String, value: &str) {
        // Check if this element already has a step (incremental typing).
        let existing_index = self.find_step_for_element(&selector_info);

        // Try to match value to an unbound column.
        let (source, matched_column) = self.match_column(value, existing_index);

        let selector = selector_info.clone().into_selector(tag);

        if let Some(idx) = existing_index {
            // Incremental typing — update existing step, preserving its navigation.
            // Discard any pending navigation state so it doesn't leak into the
            // next step. The user is still in the same field, not navigating.
            self.pending_anchor = None;
            self.pending_keys.clear();

            // Update the timestamp so the *next* step's delay is measured from
            // the latest keystroke, but preserve step_delays[0] = zero (the
            // first step has no predecessor to measure from).
            let delay = self.record_step_time();

            // Preserve the navigation path that was captured when the step was first created.
            let existing_nav = match &self.steps[idx] {
                Step::Type { navigation, .. } | Step::Click { navigation, .. } => {
                    navigation.clone()
                }
                Step::WaitForNavigation => None,
            };

            // If the old step was column-bound, unbind it first.
            self.unbind_step(idx);

            let step = Step::Type {
                selector,
                source: source.clone(),
                navigation: existing_nav,
            };

            self.steps[idx] = step.clone();
            self.step_selectors[idx] = Some(selector_info);
            if idx > 0 {
                self.step_delays[idx] = delay;
            }

            if let Some(col) = matched_column {
                self.column_bindings[col] = Some(idx);
                self.emit(TrainingEvent::ColumnBound {
                    column: col,
                    step_index: idx,
                });
            }

            self.emit(TrainingEvent::StepUpdated { index: idx, step });
        } else {
            // New step — attach pending navigation if the user tabbed here.
            let navigation = if self.pending_keys.is_empty() {
                None
            } else {
                Some(NavigationPath {
                    anchor: self.pending_anchor.take(),
                    keys: mem::take(&mut self.pending_keys),
                })
            };

            // This element becomes the anchor for subsequent tab navigation,
            // whether or not we consumed pending keys. If the user tabs from
            // this field to the next, this element is the right starting point.
            self.pending_anchor = Some(selector.clone());

            let step = Step::Type {
                selector,
                source: source.clone(),
                navigation,
            };

            let delay = self.record_step_time();
            let index = self.steps.len();
            self.steps.push(step.clone());
            self.step_selectors.push(Some(selector_info));
            self.step_delays.push(delay);

            if let Some(col) = matched_column {
                self.column_bindings[col] = Some(index);
                self.emit(TrainingEvent::ColumnBound {
                    column: col,
                    step_index: index,
                });
            }

            self.emit(TrainingEvent::StepRecorded { index, step });
        }

        // Check if row is now complete.
        if self.is_row_complete() {
            // Snapshot row-end delay now — the gap from the last step to
            // when all fields were mapped. This avoids capturing the time
            // the user spends reviewing the TUI before pressing Enter.
            self.row_end_delay = self.last_step_time.map_or(Duration::ZERO, |t| t.elapsed());
            self.emit(TrainingEvent::RowComplete {
                row_index: self.current_row,
            });
        }
    }

    fn handle_navigation(&mut self, _url: String) {
        // The previous page's elements no longer exist — any pending anchor
        // or tab keys from that page are stale and must be discarded.
        self.pending_anchor = None;
        self.pending_keys.clear();

        let delay = self.record_step_time();
        let step = Step::WaitForNavigation;
        let index = self.steps.len();
        self.steps.push(step.clone());
        self.step_selectors.push(None);
        self.step_delays.push(delay);
        self.emit(TrainingEvent::StepRecorded { index, step });
    }

    fn handle_advance_row(&mut self) {
        if self.current_row + 1 >= self.data.row_count() {
            self.emit(TrainingEvent::Error("Already on the last row".to_owned()));
            return;
        }

        self.current_row += 1;
        self.emit(TrainingEvent::RowAdvanced {
            row_index: self.current_row,
        });

        // Check for empty cells and new fields in the new row.
        if let Some(row) = self.data.row(self.current_row) {
            let row = row.to_vec(); // Clone to avoid borrow issues.
            for (col, cell) in row.iter().enumerate() {
                if let Some(Some(_step_idx)) = self.column_bindings.get(col) {
                    // Column is bound — check if the new cell is empty.
                    if cell.is_empty() && !self.empty_cell_rules.contains_key(&col) {
                        self.emit(TrainingEvent::EmptyCellEncountered {
                            column: col,
                            row_index: self.current_row,
                        });
                    }
                } else if !cell.is_empty() {
                    // Column is unbound but has a value — new field encountered.
                    self.emit(TrainingEvent::NewFieldEncountered {
                        column: col,
                        value: cell.clone(),
                    });
                }
            }
        }
    }

    fn handle_new_field(&mut self, column: usize) {
        // Bounds-check first; panic would be wrong for user-driven input.
        if column >= self.column_bindings.len() {
            self.emit(TrainingEvent::Error(format!(
                "column index {column} out of range (have {})",
                self.column_bindings.len(),
            )));
            return;
        }

        // Collect already-bound step indices so we can skip them.
        let already_bound: HashSet<usize> =
            self.column_bindings.iter().filter_map(|b| *b).collect();

        // Find the most recent Type step that is not already bound to a column.
        // Clone the selector out so we can mutate self.steps afterwards.
        let found = self.steps.iter().enumerate().rev().find_map(|(i, step)| {
            if already_bound.contains(&i) {
                return None;
            }
            if let Step::Type {
                selector,
                navigation,
                ..
            } = step
            {
                Some((i, selector.clone(), navigation.clone()))
            } else {
                None
            }
        });

        if let Some((idx, selector, existing_nav)) = found {
            // Update the step's source to the new column, preserving its navigation path.
            let updated = Step::Type {
                selector,
                source: ValueSource::Column { index: column },
                navigation: existing_nav,
            };
            self.steps[idx] = updated.clone();

            self.column_bindings[column] = Some(idx);
            self.emit(TrainingEvent::ColumnBound {
                column,
                step_index: idx,
            });
            self.emit(TrainingEvent::StepUpdated {
                index: idx,
                step: updated,
            });
        }
    }

    /// Find the index of an existing step that targets the same DOM element.
    fn find_step_for_element(&self, info: &SelectorInfo) -> Option<usize> {
        self.step_selectors
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, stored)| stored.as_ref().filter(|s| s.same_element(info)).map(|_| i))
    }

    /// Try to match a value to the next unbound column in the current row.
    ///
    /// Column-order training: only the leftmost unbound column is considered.
    /// If the value matches that column's cell, it becomes a column binding.
    /// Otherwise the value is treated as a literal.
    ///
    /// If `updating_step` is `Some`, columns currently bound to that step are
    /// considered "available" (since we're about to replace the step).
    fn match_column(
        &self,
        value: &str,
        updating_step: Option<usize>,
    ) -> (ValueSource, Option<usize>) {
        let Some(row) = self.data.row(self.current_row) else {
            return (
                ValueSource::Literal {
                    value: value.to_owned(),
                },
                None,
            );
        };

        // Find the leftmost unbound column (considering updating_step as available).
        let next_unbound = row.iter().enumerate().find(|(col, _cell)| {
            let binding = self.column_bindings.get(*col).copied().flatten();
            binding.is_none() || binding == updating_step
        });

        if let Some((col, cell)) = next_unbound
            && cell == value
        {
            return (ValueSource::Column { index: col }, Some(col));
        }

        (
            ValueSource::Literal {
                value: value.to_owned(),
            },
            None,
        )
    }

    /// Remove any column binding that points to the given step index.
    fn unbind_step(&mut self, step_index: usize) {
        for binding in &mut self.column_bindings {
            if *binding == Some(step_index) {
                *binding = None;
            }
        }
    }

    fn emit(&self, event: TrainingEvent) {
        // Ignore send errors — the receiver may have been dropped.
        let _ = self.event_tx.send(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{data, workflow::Resolution};

    /// Create a test data set from TSV text.
    fn test_data(tsv: &str) -> DataSet {
        data::from_delimited_str(tsv, data::Delimiter::Tab, true).unwrap()
    }

    /// Create a `SelectorInfo` with just an id and css.
    fn sel(id: &str, css: &str) -> SelectorInfo {
        SelectorInfo {
            id: Some(id.to_owned()),
            css: Some(css.to_owned()),
            xpath: Some(format!("//*[@id=\"{id}\"]")),
            text_content: None,
        }
    }

    /// Drain all available events from the receiver.
    fn drain_events(rx: &mut mpsc::UnboundedReceiver<TrainingEvent>) -> Vec<TrainingEvent> {
        let mut events = Vec::new();
        while let Ok(e) = rx.try_recv() {
            events.push(e);
        }
        events
    }

    fn has_event<F>(events: &[TrainingEvent], pred: F) -> bool
    where
        F: Fn(&TrainingEvent) -> bool,
    {
        events.iter().any(pred)
    }

    // -- Tests --

    #[test]
    fn click_records_a_step() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserClick {
            selector_info: sel("submit-btn", "#submit-btn"),
            tag: "BUTTON".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            TrainingEvent::StepRecorded {
                index: 0,
                step: Step::Click { .. }
            }
        ));
        assert_eq!(core.steps().len(), 1);
    }

    #[test]
    fn input_binds_a_column() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::ColumnBound {
                column: 0,
                step_index: 0
            }
        )));
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::StepRecorded { index: 0, .. }
        )));

        // Verify the step source is Column.
        match &core.steps()[0] {
            Step::Type { source, .. } => {
                assert_eq!(*source, ValueSource::Column { index: 0 });
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn input_with_no_match_is_literal() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("other", "#other"),
            tag: "INPUT".to_owned(),
            value: "unmatched-value".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(!has_event(&events, |e| matches!(
            e,
            TrainingEvent::ColumnBound { .. }
        )));

        match &core.steps()[0] {
            Step::Type { source, .. } => {
                assert_eq!(
                    *source,
                    ValueSource::Literal {
                        value: "unmatched-value".to_owned()
                    }
                );
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn incremental_typing_updates_step() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Type "Al" — no column match.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Al".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::StepRecorded { index: 0, .. }
        )));

        // Type "Ali" — still no match, but same element → update.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Ali".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::StepUpdated { index: 0, .. }
        )));

        // Only one step should exist.
        assert_eq!(core.steps().len(), 1);
    }

    #[test]
    fn incremental_typing_rebinds_column() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Type "Al" — literal.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Al".to_owned(),
        });
        let _ = drain_events(&mut rx);

        assert!(core.bound_columns()[0].is_none());

        // Type "Alice" — matches column 0.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::ColumnBound {
                column: 0,
                step_index: 0
            }
        )));

        assert_eq!(core.bound_columns()[0], Some(0));
        assert_eq!(core.steps().len(), 1);

        match &core.steps()[0] {
            Step::Type { source, .. } => {
                assert_eq!(*source, ValueSource::Column { index: 0 });
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn row_completion() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind column 0.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);
        assert!(!core.is_row_complete());

        // Bind column 1.
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::RowComplete { row_index: 0 }
        )));
        assert!(core.is_row_complete());
    }

    #[test]
    fn advance_row_increments() {
        let ds = test_data("name\nAlice\nBob\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        assert_eq!(core.current_row_index(), 0);
        core.process(Command::AdvanceRow);
        assert_eq!(core.current_row_index(), 1);

        let _ = drain_events(&mut rx);
    }

    #[test]
    fn advance_row_detects_empty_cell() {
        // Row 0: Alice, 30 — Row 1: Bob, (empty)
        let ds = test_data("name\tage\nAlice\t30\nBob\t\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind both columns on row 0.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Advance to row 1.
        core.process(Command::AdvanceRow);

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::EmptyCellEncountered {
                column: 1,
                row_index: 1
            }
        )));
    }

    #[test]
    fn advance_row_detects_new_field() {
        // Row 0: Alice, (empty) — Row 1: Bob, 25
        // Only bind column 0 on row 0. Column 1 is unbound.
        let ds = test_data("name\tage\nAlice\t\nBob\t25\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind column 0 only.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Advance to row 1.
        core.process(Command::AdvanceRow);

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::NewFieldEncountered { column: 1, .. }
        )));
    }

    #[test]
    fn speed_change() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::SetSpeed(PlaybackSpeed::Run));

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::SpeedChanged(PlaybackSpeed::Run)
        )));
        assert_eq!(core.speed(), PlaybackSpeed::Run);
    }

    #[test]
    fn navigation_records_a_step() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserNavigation {
            url: "https://example.com/form".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::StepRecorded {
                index: 0,
                step: Step::WaitForNavigation
            }
        )));
    }

    #[test]
    fn row_complete_with_empty_cells_not_required() {
        // Row has an empty cell — that column doesn't need binding for completion.
        let ds = test_data("name\tage\nAlice\t\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind only column 0 (column 1 is empty).
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::RowComplete { row_index: 0 }
        )));
        assert!(core.is_row_complete());
    }

    #[test]
    fn advance_past_last_row_emits_error() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::AdvanceRow);

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(e, TrainingEvent::Error(_))));
        // Row should not have changed.
        assert_eq!(core.current_row_index(), 0);
    }

    // -- HandleNewField tests --

    #[test]
    fn handle_new_field_binds_column_and_updates_source() {
        // Column 1 is unbound; a Type step exists with a literal value.
        let ds = test_data("name\tage\nAlice\t\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Record a Type step whose value doesn't match any column.
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "something-else".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Issue HandleNewField to bind column 1 to that step.
        core.process(Command::HandleNewField { column: 1 });

        let events = drain_events(&mut rx);
        assert!(
            has_event(&events, |e| matches!(
                e,
                TrainingEvent::ColumnBound {
                    column: 1,
                    step_index: 0
                }
            )),
            "expected ColumnBound event",
        );

        // The step's source should now be Column { index: 1 }.
        match &core.steps()[0] {
            Step::Type { source, .. } => {
                assert_eq!(*source, ValueSource::Column { index: 1 });
            }
            _ => panic!("expected Type step"),
        }

        // The binding should be recorded.
        assert_eq!(core.bound_columns()[1], Some(0));
    }

    #[test]
    fn handle_new_field_skips_already_bound_steps() {
        // Two Type steps; first is bound to column 0, second is unbound.
        let ds = test_data("first\tsecond\nAlice\t\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind column 0 to step 0.
        core.process(Command::BrowserInput {
            selector_info: sel("first-field", "#first-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        // Record a second Type step (not bound).
        core.process(Command::BrowserInput {
            selector_info: sel("second-field", "#second-field"),
            tag: "INPUT".to_owned(),
            value: "unmatched".to_owned(),
        });
        let _ = drain_events(&mut rx);

        assert_eq!(core.bound_columns()[0], Some(0)); // step 0 bound to col 0
        assert_eq!(core.bound_columns()[1], None); // col 1 unbound

        // HandleNewField for column 1 should bind to step 1 (not step 0).
        core.process(Command::HandleNewField { column: 1 });

        let events = drain_events(&mut rx);
        assert!(
            has_event(&events, |e| matches!(
                e,
                TrainingEvent::ColumnBound {
                    column: 1,
                    step_index: 1
                }
            )),
            "expected binding to step 1, not step 0",
        );
        assert_eq!(core.bound_columns()[1], Some(1));
    }

    #[test]
    fn handle_new_field_out_of_range_emits_error() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Record a Type step.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "unmatched".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Column 99 is out of range (only 1 column exists).
        core.process(Command::HandleNewField { column: 99 });

        let events = drain_events(&mut rx);
        assert!(
            has_event(&events, |e| matches!(e, TrainingEvent::Error(_))),
            "expected an Error event",
        );
        // Bindings should be unmodified.
        assert_eq!(core.bound_columns()[0], None);
    }

    // -- HandleEmptyCell tests --

    #[test]
    fn handle_empty_cell_stores_rule() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::HandleEmptyCell {
            column: 0,
            rule: EmptyCellRule::Skip,
        });
        core.process(Command::HandleEmptyCell {
            column: 1,
            rule: EmptyCellRule::Default {
                value: "N/A".to_owned(),
            },
        });

        assert_eq!(core.empty_cell_rules().get(&0), Some(&EmptyCellRule::Skip));
        assert_eq!(
            core.empty_cell_rules().get(&1),
            Some(&EmptyCellRule::Default {
                value: "N/A".to_owned()
            }),
        );
    }

    // -- build_workflow tests --

    // -- Column-order training tests --

    #[test]
    fn column_order_skips_non_next_column_match() {
        // Data: name=Alice, age=30. If user types "30" first,
        // it should NOT bind to column 1 — column 0 is the next unbound.
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });

        let events = drain_events(&mut rx);
        // Should NOT emit a ColumnBound event — "30" matches column 1
        // but column 0 is the next unbound column.
        assert!(
            !has_event(&events, |e| matches!(e, TrainingEvent::ColumnBound { .. })),
            "should not bind a non-next column",
        );

        // Step should be a literal.
        match &core.steps()[0] {
            Step::Type { source, .. } => {
                assert_eq!(
                    *source,
                    ValueSource::Literal {
                        value: "30".to_owned()
                    }
                );
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn column_order_binds_leftmost_unbound() {
        // Bind column 0 first, then column 1 becomes the next unbound.
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Bind column 0.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);
        assert_eq!(core.bound_columns()[0], Some(0));

        // Now "30" should bind to column 1 (the new leftmost unbound).
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(has_event(&events, |e| matches!(
            e,
            TrainingEvent::ColumnBound {
                column: 1,
                step_index: 1
            }
        )));
        assert_eq!(core.bound_columns()[1], Some(1));
    }

    #[test]
    fn column_order_nudge_rebinds_after_out_of_order_fill() {
        // User fills age (col 1) first — literal. Then fills name (col 0) — binds.
        // Then nudges age field — now col 1 is leftmost unbound, so it binds.
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Type "30" into age field — col 0 is next unbound, no match → literal.
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });
        let _ = drain_events(&mut rx);
        assert!(core.bound_columns()[1].is_none());

        // Type "Alice" into name field — col 0 is next unbound, matches → binds.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);
        assert_eq!(core.bound_columns()[0], Some(1));

        // Nudge age field (delete and retype) — col 1 is now leftmost unbound.
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });

        let events = drain_events(&mut rx);
        assert!(
            has_event(&events, |e| matches!(
                e,
                TrainingEvent::ColumnBound {
                    column: 1,
                    step_index: 0
                }
            )),
            "nudge should bind col 1 to the existing age step",
        );
        assert!(core.is_row_complete());
    }

    // -- build_workflow tests --

    #[test]
    fn build_workflow_captures_state() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        let wf = core.build_workflow(None);
        assert_eq!(wf.column_count, 2);
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.column_bindings[0], Some(0));
        assert_eq!(wf.column_bindings[1], None);
    }

    // -- Delay capture tests --

    #[test]
    fn first_step_has_zero_delay() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        let wf = core.build_workflow(None);
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.step_delays.len(), 1);
        assert_eq!(wf.step_delays[0], std::time::Duration::ZERO);
    }

    #[test]
    fn step_delays_parallel_steps() {
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });
        core.process(Command::BrowserClick {
            selector_info: sel("submit-btn", "#submit-btn"),
            tag: "BUTTON".to_owned(),
        });
        let _ = drain_events(&mut rx);

        let wf = core.build_workflow(None);
        assert_eq!(wf.steps.len(), 3);
        assert_eq!(wf.step_delays.len(), 3);
        // First step is always zero.
        assert_eq!(wf.step_delays[0], std::time::Duration::ZERO);
        // Subsequent delays are non-negative (wall-clock elapsed).
        // We can't assert exact values, but they must be >= 0 (always true).
    }

    #[test]
    fn build_workflow_includes_row_end_delay() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        let wf = core.build_workflow(None);
        // row_end_delay is the time elapsed since the last step was recorded.
        // It should be a non-negative duration (always true).
        let _ = wf.row_end_delay; // type check: Duration
    }

    #[test]
    fn build_workflow_row_end_delay_zero_when_no_steps() {
        let ds = test_data("name\nAlice\n");
        let (tx, _rx) = mpsc::unbounded_channel();
        let core = TrainingCore::new(ds, tx);

        let wf = core.build_workflow(None);
        assert_eq!(wf.row_end_delay, std::time::Duration::ZERO);
        assert!(wf.step_delays.is_empty());
    }

    #[test]
    fn incremental_typing_updates_delay() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // First keystrokes.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Al".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Final value — same element, so step is updated.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        let wf = core.build_workflow(None);
        // Only one step: it was updated, not duplicated.
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.step_delays.len(), 1);
    }

    // -- Navigation capture tests --

    #[test]
    fn click_then_tabs_then_input_attaches_navigation() {
        // Click sets anchor, tabs accumulate, input on new element gets NavigationPath.
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserClick {
            selector_info: sel("submit-btn", "#submit-btn"),
            tag: "BUTTON".to_owned(),
        });
        core.process(Command::BrowserTab { shift: false });
        core.process(Command::BrowserTab { shift: false });
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // step 0 = Click(submit-btn), step 1 = Type(name-field)
        assert_eq!(core.steps().len(), 2);
        match &core.steps()[1] {
            Step::Type { navigation, .. } => {
                let nav = navigation.as_ref().expect("expected NavigationPath");
                assert!(nav.anchor.is_some(), "anchor should be submit-btn");
                assert_eq!(nav.keys, vec![NavKey::Tab, NavKey::Tab]);
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn tabs_then_input_no_prior_click_has_none_anchor() {
        // Tabs from URL bar (no prior click) → anchor = None.
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserTab { shift: false });
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        assert_eq!(core.steps().len(), 1);
        match &core.steps()[0] {
            Step::Type { navigation, .. } => {
                let nav = navigation.as_ref().expect("expected NavigationPath");
                assert!(nav.anchor.is_none(), "no prior click → anchor = None");
                assert_eq!(nav.keys, vec![NavKey::Tab]);
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn direct_click_into_field_then_type_has_no_navigation() {
        // User clicks directly into a field (no tabs) → navigation = None.
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Click on the field itself.
        core.process(Command::BrowserClick {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
        });
        // Input on the same element → replaces Click step (incremental-typing path).
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // The Click step is replaced by the Type step (same element).
        assert_eq!(core.steps().len(), 1);
        match &core.steps()[0] {
            Step::Type { navigation, .. } => {
                assert!(navigation.is_none(), "direct click → no navigation");
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn incremental_typing_does_not_overwrite_navigation() {
        // Navigation captured on first input must survive subsequent keystrokes.
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserTab { shift: false });
        // First keystroke — new step, navigation captured.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Al".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Second keystroke on the same element — incremental update.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        assert_eq!(core.steps().len(), 1);
        match &core.steps()[0] {
            Step::Type { navigation, .. } => {
                let nav = navigation.as_ref().expect("navigation should be preserved");
                assert!(nav.anchor.is_none());
                assert_eq!(nav.keys, vec![NavKey::Tab]);
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn tabs_then_click_attaches_navigation_to_click_step() {
        // User tabs to a button and clicks it → Click step has NavigationPath.
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserTab { shift: false });
        core.process(Command::BrowserTab { shift: true });
        core.process(Command::BrowserClick {
            selector_info: sel("submit-btn", "#submit-btn"),
            tag: "BUTTON".to_owned(),
        });
        let _ = drain_events(&mut rx);

        assert_eq!(core.steps().len(), 1);
        match &core.steps()[0] {
            Step::Click { navigation, .. } => {
                let nav = navigation
                    .as_ref()
                    .expect("expected NavigationPath on Click");
                assert!(nav.anchor.is_none(), "no prior click → anchor = None");
                assert_eq!(nav.keys, vec![NavKey::Tab, NavKey::ShiftTab]);
            }
            _ => panic!("expected Click step"),
        }
    }

    #[test]
    fn tabs_with_no_subsequent_interaction_are_discarded() {
        // User tabs around, then clicks a new element → tabs are consumed by the click.
        // Pending state resets cleanly and subsequent inputs have no stale keys.
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Tab around (these tabs end up on the click step, not on a later input).
        core.process(Command::BrowserTab { shift: false });
        core.process(Command::BrowserTab { shift: false });

        // Click — consumes the tabs as its own NavigationPath.
        core.process(Command::BrowserClick {
            selector_info: sel("some-btn", "#some-btn"),
            tag: "BUTTON".to_owned(),
        });

        // No more tabs — direct input into a field.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // step 0 = Click (with the two tabs), step 1 = Type (no navigation).
        assert_eq!(core.steps().len(), 2);
        match &core.steps()[1] {
            Step::Type { navigation, .. } => {
                assert!(
                    navigation.is_none(),
                    "tabs were consumed by the click; Type step has no navigation"
                );
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn tab_then_click_then_type_preserves_navigation_on_same_element() {
        // User tabs to an input field, browser fires click, then user types.
        // The Click step has the navigation. When replaced by Type (same element),
        // the navigation must be preserved.
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Tab to the field.
        core.process(Command::BrowserTab { shift: false });
        core.process(Command::BrowserTab { shift: false });

        // Click on the field (browser fires click on focus).
        core.process(Command::BrowserClick {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
        });

        // Type into the same field — replaces Click with Type.
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Should be one step (Type replaced Click for same element).
        assert_eq!(core.steps().len(), 1);
        match &core.steps()[0] {
            Step::Type { navigation, .. } => {
                let nav = navigation
                    .as_ref()
                    .expect("navigation from Click should be preserved on Type");
                assert!(nav.anchor.is_none(), "no prior click → anchor = None");
                assert_eq!(nav.keys, vec![NavKey::Tab, NavKey::Tab]);
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn multi_field_tab_chain_uses_previous_field_as_anchor() {
        // click → tab → type A → tab → type B
        // Field B's anchor should be field A, not None.
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Click a landmark.
        core.process(Command::BrowserClick {
            selector_info: sel("landmark", "#landmark"),
            tag: "BUTTON".to_owned(),
        });

        // Tab to field A, type.
        core.process(Command::BrowserTab { shift: false });
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });

        // Tab to field B, type.
        core.process(Command::BrowserTab { shift: false });
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // step 0 = Click(landmark), step 1 = Type(name), step 2 = Type(age)
        assert_eq!(core.steps().len(), 3);

        // Field A's anchor should be the landmark.
        match &core.steps()[1] {
            Step::Type { navigation, .. } => {
                let nav = navigation.as_ref().expect("field A should have navigation");
                assert!(nav.anchor.is_some(), "anchor should be the landmark");
                assert_eq!(nav.keys, vec![NavKey::Tab]);
            }
            _ => panic!("expected Type step"),
        }

        // Field B's anchor should be field A (name-field), not None.
        match &core.steps()[2] {
            Step::Type { navigation, .. } => {
                let nav = navigation.as_ref().expect("field B should have navigation");
                let anchor = nav.anchor.as_ref().expect("anchor should be field A");
                assert!(
                    anchor
                        .strategies
                        .iter()
                        .any(|r| matches!(r, Resolution::Id { id } if id == "name-field")),
                    "anchor should reference name-field",
                );
                assert_eq!(nav.keys, vec![NavKey::Tab]);
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn incremental_typing_clears_pending_navigation() {
        // Tabs accumulated before re-entering an existing field must not leak
        // into the next new step.
        let ds = test_data("name\tage\nAlice\t30\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Type in field A (new step).
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });

        // Tab, then re-type into field A (incremental typing — same element).
        core.process(Command::BrowserTab { shift: false });
        core.process(Command::BrowserInput {
            selector_info: sel("name-field", "#name-field"),
            tag: "INPUT".to_owned(),
            value: "Alice!".to_owned(),
        });

        // Now type into field B (new step) — should NOT have the stale tab.
        core.process(Command::BrowserInput {
            selector_info: sel("age-field", "#age-field"),
            tag: "INPUT".to_owned(),
            value: "30".to_owned(),
        });
        let _ = drain_events(&mut rx);

        assert_eq!(core.steps().len(), 2);
        match &core.steps()[1] {
            Step::Type { navigation, .. } => {
                assert!(
                    navigation.is_none(),
                    "stale tab should not leak to new step",
                );
            }
            _ => panic!("expected Type step"),
        }
    }

    #[test]
    fn page_navigation_clears_pending_state() {
        // Tabs and clicks from the previous page must not survive a navigation.
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        // Click and tab on page 1.
        core.process(Command::BrowserClick {
            selector_info: sel("old-btn", "#old-btn"),
            tag: "BUTTON".to_owned(),
        });
        core.process(Command::BrowserTab { shift: false });

        // Navigate to a new page.
        core.process(Command::BrowserNavigation {
            url: "https://example.com/page2".to_owned(),
        });

        // Tab and type on the new page.
        core.process(Command::BrowserTab { shift: false });
        core.process(Command::BrowserInput {
            selector_info: sel("new-field", "#new-field"),
            tag: "INPUT".to_owned(),
            value: "Alice".to_owned(),
        });
        let _ = drain_events(&mut rx);

        // Find the Type step (after Click and WaitForNavigation steps).
        let type_step = core
            .steps()
            .iter()
            .find(|s| matches!(s, Step::Type { .. }))
            .expect("should have a Type step");

        match type_step {
            Step::Type { navigation, .. } => {
                let nav = navigation.as_ref().expect("should have navigation");
                // Anchor should be None (old-btn was on a different page and
                // was cleared by navigation). Only the post-navigation tab counts.
                assert!(nav.anchor.is_none(), "old page anchor should be cleared");
                assert_eq!(nav.keys, vec![NavKey::Tab]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn navigation_step_captures_delay() {
        let ds = test_data("name\nAlice\n");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut core = TrainingCore::new(ds, tx);

        core.process(Command::BrowserNavigation {
            url: "https://example.com/page".to_owned(),
        });
        let _ = drain_events(&mut rx);

        let wf = core.build_workflow(None);
        assert_eq!(wf.steps.len(), 1);
        assert_eq!(wf.step_delays.len(), 1);
        // First step is always zero.
        assert_eq!(wf.step_delays[0], std::time::Duration::ZERO);
    }
}
