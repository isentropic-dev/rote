// Playback engine: orchestrates row iteration, step execution, and control flow.

use std::collections::BTreeMap;
use std::future::Future;

use tokio::sync::{broadcast, mpsc};

use crate::cdp::{Browser, Event as CdpEvent};
use crate::data::DataSet;
use crate::workflow::{EmptyCellRule, PlaybackSpeed, Step, Workflow};

use super::execute::{self, StepOutcome};
use super::{ErrorAction, PlaybackConfig, PlaybackControl, PlaybackError, PlaybackEvent};

// ─── Pure gate-decision helpers ───────────────────────────────────────────

/// Whether the engine should pause for confirmation after `step` at `speed`.
#[must_use]
fn needs_step_gate(speed: PlaybackSpeed, step: &Step) -> bool {
    match speed {
        PlaybackSpeed::Step => match step {
            Step::Type { .. } => true,
            Step::Click { .. } | Step::WaitForNavigation => false,
        },
        PlaybackSpeed::Walk | PlaybackSpeed::Run => false,
    }
}

/// Whether the engine should pause for confirmation after finishing a row.
#[must_use]
fn needs_row_gate(speed: PlaybackSpeed) -> bool {
    speed == PlaybackSpeed::Walk
}

// ─── Internal executor abstraction ───────────────────────────────────────

/// Abstracts step execution so the engine loop can be tested without a browser.
pub(crate) trait Executor {
    /// Subscribe to CDP events, if supported by this executor.
    ///
    /// Returns `None` for mock executors, `Some(receiver)` for the real browser
    /// executor. The engine calls this before executing a step that precedes a
    /// [`Step::WaitForNavigation`] step, so the subscription is in place before
    /// the navigation-triggering action runs.
    fn subscribe(&self) -> Option<broadcast::Receiver<CdpEvent>>;

    /// Execute `step` against `row` with the given empty-cell rules.
    ///
    /// `pre_subscribed` is a CDP event receiver that was subscribed *before*
    /// the preceding step executed, used by [`Step::WaitForNavigation`] to
    /// avoid a race between the navigation trigger and event subscription.
    ///
    /// # Errors
    ///
    /// Implementation-defined; see [`execute_step`](super::execute::execute_step).
    fn run_step(
        &self,
        step: &Step,
        row: &[String],
        rules: &BTreeMap<usize, EmptyCellRule>,
        pre_subscribed: Option<broadcast::Receiver<CdpEvent>>,
    ) -> impl Future<Output = Result<StepOutcome, PlaybackError>>;
}

/// Production executor that drives a real browser via CDP.
pub(crate) struct BrowserExecutor<'b> {
    browser: &'b Browser,
    config: PlaybackConfig,
}

impl<'b> BrowserExecutor<'b> {
    pub(crate) fn new(browser: &'b Browser, config: PlaybackConfig) -> Self {
        Self { browser, config }
    }
}

impl Executor for BrowserExecutor<'_> {
    fn subscribe(&self) -> Option<broadcast::Receiver<CdpEvent>> {
        Some(self.browser.subscribe())
    }

    async fn run_step(
        &self,
        step: &Step,
        row: &[String],
        rules: &BTreeMap<usize, EmptyCellRule>,
        pre_subscribed: Option<broadcast::Receiver<CdpEvent>>,
    ) -> Result<StepOutcome, PlaybackError> {
        execute::execute_step(self.browser, step, row, rules, pre_subscribed, &self.config).await
    }
}

// ─── Row outcome ─────────────────────────────────────────────────────────

/// Internal result of processing a single row.
enum RowOutcome {
    /// All steps succeeded; advance to the next row.
    Completed,
    /// Row was skipped (error + `SkipRow`).
    Skipped,
    /// Row should be retried from step 0.
    Retry,
    /// User requested a full stop.
    Stop,
}

// ─── Public result type ───────────────────────────────────────────────────

/// Summary of a completed playback run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackResult {
    /// Rows that completed every step successfully.
    pub rows_completed: usize,
    /// Rows that were skipped (error + `SkipRow` action).
    pub rows_skipped: usize,
}

// ─── Engine ───────────────────────────────────────────────────────────────

/// Orchestrates playback of a [`Workflow`] against a [`DataSet`].
///
/// Communicate with the engine at runtime via the channels returned by [`PlaybackEngine::new`].
#[allow(clippy::module_name_repetitions)]
pub struct PlaybackEngine {
    workflow: Workflow,
    data: DataSet,
    /// Row to start (or resume) from.
    current_row: usize,
    speed: PlaybackSpeed,
    /// Multiplier applied to delay sleeps.
    ///
    /// 2.0 = twice as fast (half the delay). Clamped to 0.25..=4.0.
    speed_multiplier: f64,
    config: PlaybackConfig,
    /// Receives control signals from the TUI / CLI.
    control_rx: mpsc::UnboundedReceiver<PlaybackControl>,
    /// Sends progress events to the TUI / CLI.
    event_tx: mpsc::UnboundedSender<PlaybackEvent>,
}

impl PlaybackEngine {
    /// Create a new engine and return the associated control/event channels.
    ///
    /// - `control_tx` — send [`PlaybackControl`] messages to drive the engine.
    /// - `event_rx` — receive [`PlaybackEvent`] messages from the engine.
    #[must_use]
    pub fn new(
        workflow: Workflow,
        data: DataSet,
        speed: PlaybackSpeed,
        start_row: usize,
    ) -> (
        Self,
        mpsc::UnboundedSender<PlaybackControl>,
        mpsc::UnboundedReceiver<PlaybackEvent>,
    ) {
        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let engine = Self {
            workflow,
            data,
            current_row: start_row,
            speed,
            speed_multiplier: 1.0,
            config: PlaybackConfig::default(),
            control_rx,
            event_tx,
        };

        (engine, control_tx, event_rx)
    }

    /// Override the default playback configuration.
    #[cfg(test)]
    pub(crate) fn set_config(&mut self, config: PlaybackConfig) {
        self.config = config;
    }

    /// Run the playback loop from [`Self::current_row`] to the last data row.
    ///
    /// Each row emits [`PlaybackEvent::RowStarted`] / [`PlaybackEvent::RowCompleted`].
    /// Each step emits [`PlaybackEvent::StepStarted`] / [`PlaybackEvent::StepCompleted`]
    /// (or [`PlaybackEvent::StepFailed`] on error).
    /// When all rows are done (or the user stops) a [`PlaybackEvent::Finished`] event is sent.
    ///
    /// # Errors
    ///
    /// - [`PlaybackError::Stopped`] — user sent [`ErrorAction::Stop`] or the control channel closed.
    /// - [`PlaybackError::Cdp`] — an unrecoverable CDP error occurred.
    pub async fn run(&mut self, browser: &Browser) -> Result<PlaybackResult, PlaybackError> {
        let executor = BrowserExecutor::new(browser, self.config.clone());
        self.run_with(executor).await
    }

    /// Internal implementation, generic over the step executor.
    pub(crate) async fn run_with<E: Executor>(
        &mut self,
        executor: E,
    ) -> Result<PlaybackResult, PlaybackError> {
        // Validate that the data has enough columns for this workflow.
        if self.data.column_count() < self.workflow.column_count {
            return Err(PlaybackError::Other(format!(
                "data has {} column(s) but workflow expects {}",
                self.data.column_count(),
                self.workflow.column_count,
            )));
        }

        let mut rows_completed = 0usize;
        let mut rows_skipped = 0usize;
        let row_count = self.data.row_count();
        let mut row_index = self.current_row;

        while row_index < row_count {
            let _ = self.event_tx.send(PlaybackEvent::RowStarted { row_index });

            // Clone the row so we can hold `&mut self` for gate/control ops.
            let row = self
                .data
                .row(row_index)
                .map(<[String]>::to_vec)
                .ok_or_else(|| PlaybackError::Other(format!("row {row_index} out of bounds")))?;

            match self.run_row(&executor, row_index, &row).await? {
                RowOutcome::Completed => {
                    let _ = self
                        .event_tx
                        .send(PlaybackEvent::RowCompleted { row_index });
                    rows_completed += 1;
                    row_index += 1;
                }
                RowOutcome::Skipped => {
                    rows_skipped += 1;
                    row_index += 1;
                }
                RowOutcome::Retry => {
                    // row_index unchanged — will re-execute the same row.
                }
                RowOutcome::Stop => {
                    let _ = self.event_tx.send(PlaybackEvent::Finished {
                        rows_completed,
                        rows_skipped,
                    });
                    return Err(PlaybackError::Stopped);
                }
            }
        }

        let _ = self.event_tx.send(PlaybackEvent::Finished {
            rows_completed,
            rows_skipped,
        });

        Ok(PlaybackResult {
            rows_completed,
            rows_skipped,
        })
    }

    /// Execute all steps for one row, returning the row-level outcome.
    async fn run_row<E: Executor>(
        &mut self,
        executor: &E,
        row_index: usize,
        row: &[String],
    ) -> Result<RowOutcome, PlaybackError> {
        let effective_step_count = self.workflow.steps.len();

        // Carries a pre-subscribed CDP event receiver from the step before a
        // WaitForNavigation step to the WaitForNavigation step itself.
        let mut pending_subscription: Option<broadcast::Receiver<CdpEvent>> = None;

        for step_index in 0..effective_step_count {
            // Clone the step to free the borrow on `self.workflow` before we
            // need `&mut self` for gate/control operations.
            let step = self.workflow.steps[step_index].clone();

            // If the *next* step is WaitForNavigation, subscribe to CDP events
            // now — before executing the current step — to avoid the race
            // between the navigation trigger (e.g. a Click) and our subscription.
            let next_is_nav = step_index + 1 < effective_step_count
                && matches!(self.workflow.steps[step_index + 1], Step::WaitForNavigation);
            if next_is_nav {
                pending_subscription = executor.subscribe();
            }

            // Hand the pre-subscribed receiver to WaitForNavigation steps.
            let pre_sub = if matches!(step, Step::WaitForNavigation) {
                pending_subscription.take()
            } else {
                None
            };

            // Sleep the per-step delay *before* executing, matching training
            // semantics: step_delays[i] is the gap before step i was recorded.
            // step_delays[0] is always zero so the first step runs immediately.
            // The speed_multiplier scales the delay: 2.0× speed = half the delay.
            if !needs_step_gate(self.speed, &step) {
                let delay = self.workflow.step_delays[step_index];
                if !delay.is_zero() {
                    let scaled = delay.mul_f64(1.0 / self.speed_multiplier);
                    tokio::time::sleep(scaled).await;
                }
            }

            let _ = self.event_tx.send(PlaybackEvent::StepStarted {
                row_index,
                step_index,
            });

            let result = executor
                .run_step(&step, row, &self.workflow.empty_cell_rules, pre_sub)
                .await;

            match result {
                Ok(StepOutcome::Skipped) => {
                    let _ = self.event_tx.send(PlaybackEvent::StepCompleted {
                        row_index,
                        step_index,
                    });
                }

                Ok(StepOutcome::Executed) => {
                    let _ = self.event_tx.send(PlaybackEvent::StepCompleted {
                        row_index,
                        step_index,
                    });

                    // Always drain pending control messages (speed changes)
                    // between steps — even in ungated modes. This is what
                    // makes "slow down from Run" work.
                    self.apply_pending_controls()?;

                    if needs_step_gate(self.speed, &step) {
                        self.wait_for_confirmation().await?;
                    }
                }

                Err(e) => {
                    let _ = self.event_tx.send(PlaybackEvent::StepFailed {
                        row_index,
                        step_index,
                        error: e.to_string(),
                    });

                    match self.wait_for_error_action().await? {
                        ErrorAction::SkipRow => return Ok(RowOutcome::Skipped),
                        // TODO: if step 0 is WaitForNavigation and we retry, the
                        // navigation event from the *previous* iteration may have
                        // already been consumed, causing WaitForNavigation to hang
                        // until its timeout. A full fix requires re-executing the
                        // triggering step or caching the navigation event.
                        ErrorAction::RetryRow => return Ok(RowOutcome::Retry),
                        ErrorAction::Stop => return Ok(RowOutcome::Stop),
                    }
                }
            }
        }

        // Drain controls before the row gate — a speed change sent during
        // the last step of the row might have switched us away from Row.
        self.apply_pending_controls()?;

        // Sleep the row-end delay when not gated, to reproduce the natural
        // pause captured between the last step and row finalization during
        // training. The speed_multiplier scales the delay.
        if !needs_row_gate(self.speed) {
            let delay = self.workflow.row_end_delay;
            if !delay.is_zero() {
                let scaled = delay.mul_f64(1.0 / self.speed_multiplier);
                tokio::time::sleep(scaled).await;
            }
        }

        // Row-level gate: wait after all steps if speed is Walk.
        if needs_row_gate(self.speed) {
            self.wait_for_confirmation().await?;
        }

        Ok(RowOutcome::Completed)
    }

    /// Drain all pending control messages without blocking.
    ///
    /// Applies [`PlaybackControl::SetSpeed`] and [`PlaybackControl::SetSpeedMultiplier`]
    /// immediately. [`PlaybackControl::Proceed`] and [`PlaybackControl::ErrorResponse`]
    /// are ignored (they only matter inside blocking gates).
    ///
    /// Called between every step so speed changes take effect even when the
    /// engine is in an ungated mode (Row, Auto).
    ///
    /// # Errors
    ///
    /// Returns [`PlaybackError::Stopped`] if the control channel is closed.
    fn apply_pending_controls(&mut self) -> Result<(), PlaybackError> {
        loop {
            match self.control_rx.try_recv() {
                Ok(PlaybackControl::SetSpeed(speed)) => {
                    self.speed = speed;
                    let _ = self.event_tx.send(PlaybackEvent::SpeedChanged(speed));
                }
                Ok(PlaybackControl::SetSpeedMultiplier(m)) => {
                    let clamped = m.clamp(0.25, 4.0);
                    self.speed_multiplier = clamped;
                    let _ = self
                        .event_tx
                        .send(PlaybackEvent::SpeedMultiplierChanged(clamped));
                }
                // Proceed and ErrorResponse are only meaningful inside blocking gates.
                Ok(PlaybackControl::Proceed | PlaybackControl::ErrorResponse(_)) => {}
                // Channel empty — done draining.
                Err(mpsc::error::TryRecvError::Empty) => return Ok(()),
                // Channel closed — treat as Stop.
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(PlaybackError::Stopped);
                }
            }
        }
    }

    /// Block at a confirmation gate until [`PlaybackControl::Proceed`] is received.
    ///
    /// While waiting, [`PlaybackControl::SetSpeed`] and [`PlaybackControl::SetSpeedMultiplier`]
    /// are handled transparently.
    ///
    /// # Errors
    ///
    /// Returns [`PlaybackError::Stopped`] if the control channel closes.
    async fn wait_for_confirmation(&mut self) -> Result<(), PlaybackError> {
        let _ = self.event_tx.send(PlaybackEvent::WaitingForConfirmation);

        loop {
            match self.control_rx.recv().await {
                Some(PlaybackControl::Proceed) => return Ok(()),

                Some(PlaybackControl::SetSpeed(speed)) => {
                    self.speed = speed;
                    let _ = self.event_tx.send(PlaybackEvent::SpeedChanged(speed));
                }

                Some(PlaybackControl::SetSpeedMultiplier(m)) => {
                    let clamped = m.clamp(0.25, 4.0);
                    self.speed_multiplier = clamped;
                    let _ = self
                        .event_tx
                        .send(PlaybackEvent::SpeedMultiplierChanged(clamped));
                }

                // Ignore error responses while at a confirmation gate.
                Some(PlaybackControl::ErrorResponse(_)) => {}

                // Channel closed — treat as Stop.
                None => return Err(PlaybackError::Stopped),
            }
        }
    }

    /// Block until a [`PlaybackControl::ErrorResponse`] is received.
    ///
    /// While waiting, [`PlaybackControl::SetSpeed`] and [`PlaybackControl::SetSpeedMultiplier`]
    /// are handled transparently.
    ///
    /// # Errors
    ///
    /// Returns [`PlaybackError::Stopped`] if the control channel closes.
    async fn wait_for_error_action(&mut self) -> Result<ErrorAction, PlaybackError> {
        loop {
            match self.control_rx.recv().await {
                Some(PlaybackControl::ErrorResponse(action)) => return Ok(action),

                Some(PlaybackControl::SetSpeed(speed)) => {
                    self.speed = speed;
                    let _ = self.event_tx.send(PlaybackEvent::SpeedChanged(speed));
                }

                Some(PlaybackControl::SetSpeedMultiplier(m)) => {
                    let clamped = m.clamp(0.25, 4.0);
                    self.speed_multiplier = clamped;
                    let _ = self
                        .event_tx
                        .send(PlaybackEvent::SpeedMultiplierChanged(clamped));
                }

                // Ignore Proceed while in error state.
                Some(PlaybackControl::Proceed) => {}

                // Channel closed — treat as Stop.
                None => return Err(PlaybackError::Stopped),
            }
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use super::*;
    use crate::workflow::{PlaybackSpeed, Resolution, Selector, Step, ValueSource, Workflow};

    // ── Mock executor ─────────────────────────────────────────────────────

    /// Always returns `Executed` — used when we don't care about step results.
    struct OkExecutor;

    impl Executor for OkExecutor {
        fn subscribe(&self) -> Option<broadcast::Receiver<CdpEvent>> {
            None
        }

        async fn run_step(
            &self,
            _step: &Step,
            _row: &[String],
            _rules: &BTreeMap<usize, EmptyCellRule>,
            _pre_subscribed: Option<broadcast::Receiver<CdpEvent>>,
        ) -> Result<StepOutcome, PlaybackError> {
            Ok(StepOutcome::Executed)
        }
    }

    /// Always returns the error stored inside.
    struct ErrExecutor(String);

    impl Executor for ErrExecutor {
        fn subscribe(&self) -> Option<broadcast::Receiver<CdpEvent>> {
            None
        }

        async fn run_step(
            &self,
            _step: &Step,
            _row: &[String],
            _rules: &BTreeMap<usize, EmptyCellRule>,
            _pre_subscribed: Option<broadcast::Receiver<CdpEvent>>,
        ) -> Result<StepOutcome, PlaybackError> {
            Err(PlaybackError::Other(self.0.clone()))
        }
    }

    // ── Step constructors ─────────────────────────────────────────────────

    fn click_step() -> Step {
        Step::Click {
            selector: Selector {
                strategies: vec![],
                tag: "BUTTON".to_owned(),
            },
            navigation: None,
        }
    }

    fn type_step() -> Step {
        Step::Type {
            selector: Selector {
                strategies: vec![],
                tag: "INPUT".to_owned(),
            },
            source: ValueSource::Literal {
                value: "x".to_owned(),
            },
            navigation: None,
        }
    }

    fn wait_for_navigation_step() -> Step {
        Step::WaitForNavigation
    }

    // ── Gate-decision pure tests ──────────────────────────────────────────

    #[test]
    fn step_speed_gates_only_type_steps() {
        assert!(!needs_step_gate(PlaybackSpeed::Step, &click_step()));
        assert!(needs_step_gate(PlaybackSpeed::Step, &type_step()));
        assert!(!needs_step_gate(
            PlaybackSpeed::Step,
            &wait_for_navigation_step()
        ));
    }

    #[test]
    fn walk_and_run_never_gate_steps() {
        for speed in [PlaybackSpeed::Walk, PlaybackSpeed::Run] {
            for step in &[click_step(), type_step(), wait_for_navigation_step()] {
                assert!(!needs_step_gate(speed, step));
            }
        }
    }

    #[test]
    fn only_walk_speed_gates_rows() {
        assert!(needs_row_gate(PlaybackSpeed::Walk));
        assert!(!needs_row_gate(PlaybackSpeed::Step));
        assert!(!needs_row_gate(PlaybackSpeed::Run));
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    fn dataset(rows: &[&str]) -> crate::data::DataSet {
        let text = format!("col\n{}\n", rows.join("\n"));
        crate::data::from_delimited_str(&text, crate::data::Delimiter::Tab, true)
            .expect("test dataset")
    }

    fn empty_workflow() -> Workflow {
        Workflow::new(
            1,
            vec![],
            vec![],
            Duration::ZERO,
            vec![],
            BTreeMap::new(),
            None,
        )
    }

    fn workflow_with_steps(steps: Vec<Step>) -> Workflow {
        let n = steps.len();
        Workflow::new(
            1,
            steps,
            vec![Duration::ZERO; n],
            Duration::ZERO,
            vec![],
            BTreeMap::new(),
            None,
        )
    }

    /// Create an engine for tests.
    ///
    /// Test workflows use zero delays, so no config override is needed.
    fn test_engine(
        workflow: Workflow,
        data: crate::data::DataSet,
        speed: PlaybackSpeed,
        start_row: usize,
    ) -> (
        PlaybackEngine,
        mpsc::UnboundedSender<PlaybackControl>,
        mpsc::UnboundedReceiver<PlaybackEvent>,
    ) {
        PlaybackEngine::new(workflow, data, speed, start_row)
    }

    /// Drain all available events from the receiver without blocking.
    fn drain_events(rx: &mut mpsc::UnboundedReceiver<PlaybackEvent>) -> Vec<PlaybackEvent> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            out.push(e);
        }
        out
    }

    // ── Row-iteration tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn empty_workflow_completes_all_rows() {
        let (mut engine, _ctrl, mut event_rx) = test_engine(
            empty_workflow(),
            dataset(&["Alice", "Bob"]),
            PlaybackSpeed::Run,
            0,
        );

        let result = engine.run_with(OkExecutor).await.unwrap();

        assert_eq!(result.rows_completed, 2);
        assert_eq!(result.rows_skipped, 0);

        let events = drain_events(&mut event_rx);
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                PlaybackEvent::RowStarted { .. } => "RowStarted",
                PlaybackEvent::RowCompleted { .. } => "RowCompleted",
                PlaybackEvent::Finished { .. } => "Finished",
                _ => "other",
            })
            .collect();
        assert_eq!(
            kinds,
            &[
                "RowStarted",
                "RowCompleted",
                "RowStarted",
                "RowCompleted",
                "Finished"
            ],
        );
    }

    #[tokio::test]
    async fn row_indices_are_correct() {
        let (mut engine, _ctrl, mut event_rx) = test_engine(
            empty_workflow(),
            dataset(&["Alice", "Bob"]),
            PlaybackSpeed::Run,
            0,
        );

        engine.run_with(OkExecutor).await.unwrap();

        let started: Vec<usize> = drain_events(&mut event_rx)
            .into_iter()
            .filter_map(|e| {
                if let PlaybackEvent::RowStarted { row_index } = e {
                    Some(row_index)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(started, &[0, 1]);
    }

    #[tokio::test]
    async fn start_row_is_respected() {
        let (mut engine, _ctrl, mut event_rx) = test_engine(
            empty_workflow(),
            dataset(&["Alice", "Bob"]),
            PlaybackSpeed::Run,
            1,
        );

        let result = engine.run_with(OkExecutor).await.unwrap();
        assert_eq!(result.rows_completed, 1);

        let started: Vec<usize> = drain_events(&mut event_rx)
            .into_iter()
            .filter_map(|e| {
                if let PlaybackEvent::RowStarted { row_index } = e {
                    Some(row_index)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(started, &[1]);
    }

    #[tokio::test]
    async fn finished_event_carries_counts() {
        let (mut engine, _ctrl, mut event_rx) = test_engine(
            empty_workflow(),
            dataset(&["Alice", "Bob"]),
            PlaybackSpeed::Run,
            0,
        );

        engine.run_with(OkExecutor).await.unwrap();

        let counts = drain_events(&mut event_rx).into_iter().find_map(|e| {
            if let PlaybackEvent::Finished {
                rows_completed,
                rows_skipped,
            } = e
            {
                Some((rows_completed, rows_skipped))
            } else {
                None
            }
        });
        assert_eq!(counts, Some((2, 0)));
    }

    // ── Step event tests ──────────────────────────────────────────────────

    #[tokio::test]
    async fn steps_emit_started_and_completed() {
        let steps = vec![click_step(), type_step()];
        let (mut engine, _ctrl, mut event_rx) = test_engine(
            workflow_with_steps(steps),
            dataset(&["x"]),
            PlaybackSpeed::Run,
            0,
        );

        engine.run_with(OkExecutor).await.unwrap();

        let events = drain_events(&mut event_rx);
        let step_events: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                PlaybackEvent::StepStarted { .. } => Some("StepStarted"),
                PlaybackEvent::StepCompleted { .. } => Some("StepCompleted"),
                _ => None,
            })
            .collect();
        // Two steps: each should have Started + Completed.
        assert_eq!(
            step_events,
            &[
                "StepStarted",
                "StepCompleted",
                "StepStarted",
                "StepCompleted"
            ]
        );
    }

    // ── Confirmation gate tests ───────────────────────────────────────────

    #[tokio::test]
    async fn row_gate_fires_and_blocks() {
        // One row, one step (click), Row speed → should wait after the row.
        let steps = vec![click_step()];
        let (mut engine, ctrl_tx, mut event_rx) = test_engine(
            workflow_with_steps(steps),
            dataset(&["x"]),
            PlaybackSpeed::Walk,
            0,
        );

        let handle = tokio::spawn(async move { engine.run_with(OkExecutor).await });

        // Give the engine time to reach the gate.
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Verify it emitted WaitingForConfirmation.
        let events = drain_events(&mut event_rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PlaybackEvent::WaitingForConfirmation))
        );

        // Proceed.
        ctrl_tx.send(PlaybackControl::Proceed).unwrap();
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn step_speed_gates_after_type_steps() {
        // Click + Type at Step speed: only the Type step gates.
        let steps = vec![click_step(), type_step()];
        let (mut engine, ctrl_tx, _event_rx) = test_engine(
            workflow_with_steps(steps),
            dataset(&["x"]),
            PlaybackSpeed::Step,
            0,
        );

        let handle = tokio::spawn(async move { engine.run_with(OkExecutor).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        // One gate (after the Type step). Click auto-advances.
        ctrl_tx.send(PlaybackControl::Proceed).unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn set_speed_during_gate_is_applied() {
        // Row speed → gate fires after the row's steps.
        let (mut engine, ctrl_tx, mut event_rx) =
            test_engine(empty_workflow(), dataset(&["x"]), PlaybackSpeed::Walk, 0);

        let handle = tokio::spawn(async move {
            let result = engine.run_with(OkExecutor).await;
            (result, engine.speed)
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        ctrl_tx
            .send(PlaybackControl::SetSpeed(PlaybackSpeed::Run))
            .unwrap();
        ctrl_tx.send(PlaybackControl::Proceed).unwrap();

        let (result, final_speed) = handle.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(final_speed, PlaybackSpeed::Run);

        let events = drain_events(&mut event_rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PlaybackEvent::SpeedChanged(PlaybackSpeed::Run)))
        );
    }

    #[tokio::test]
    async fn channel_close_stops_engine_at_gate() {
        let (mut engine, ctrl_tx, _event_rx) =
            test_engine(empty_workflow(), dataset(&["x"]), PlaybackSpeed::Walk, 0);

        let handle = tokio::spawn(async move { engine.run_with(OkExecutor).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(ctrl_tx);

        let result = handle.await.unwrap();
        assert!(matches!(result, Err(PlaybackError::Stopped)));
    }

    // ── Speed change in ungated mode (the primary bug fix) ──────────────

    #[tokio::test]
    async fn speed_change_at_run_takes_effect_between_steps() {
        // Many type steps at Run speed across multiple rows. Send SetSpeed(Step)
        // early — the engine must gate on the next Type step instead of running
        // through all rows.
        let steps = vec![type_step()];
        let (mut engine, ctrl_tx, mut event_rx) = test_engine(
            workflow_with_steps(steps),
            dataset(&["a", "b", "c", "d", "e"]),
            PlaybackSpeed::Run,
            0,
        );

        // Pre-load the speed change so it's in the channel before the engine
        // even starts. The engine must pick it up in apply_pending_controls.
        ctrl_tx
            .send(PlaybackControl::SetSpeed(PlaybackSpeed::Step))
            .unwrap();

        let handle = tokio::spawn(async move { engine.run_with(OkExecutor).await });

        // Engine should gate after the first Type step (now at Step speed).
        tokio::time::sleep(Duration::from_millis(50)).await;

        let events = drain_events(&mut event_rx);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PlaybackEvent::SpeedChanged(PlaybackSpeed::Step))),
            "engine should have applied the speed change",
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, PlaybackEvent::WaitingForConfirmation)),
            "engine should have gated after switching to Step speed",
        );

        // Proceed through remaining rows.
        for _ in 0..5 {
            let _ = ctrl_tx.send(PlaybackControl::Proceed);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    // ── Error-handling tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn skip_row_advances_and_counts_skipped() {
        let steps = vec![click_step()];
        let (mut engine, ctrl_tx, mut event_rx) = test_engine(
            workflow_with_steps(steps),
            dataset(&["x", "y"]),
            PlaybackSpeed::Run,
            0,
        );

        let handle =
            tokio::spawn(async move { engine.run_with(ErrExecutor("boom".to_owned())).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        // Both rows fail; skip each.
        ctrl_tx
            .send(PlaybackControl::ErrorResponse(ErrorAction::SkipRow))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        ctrl_tx
            .send(PlaybackControl::ErrorResponse(ErrorAction::SkipRow))
            .unwrap();

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.rows_completed, 0);
        assert_eq!(result.rows_skipped, 2);

        let events = drain_events(&mut event_rx);
        let failed_count = events
            .iter()
            .filter(|e| matches!(e, PlaybackEvent::StepFailed { .. }))
            .count();
        assert_eq!(failed_count, 2);
    }

    #[tokio::test]
    async fn retry_row_re_executes() {
        // First error → Retry; second error → Skip.
        // Use a counter to distinguish first/second call.
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct CountingExecutor(Arc<AtomicUsize>);

        impl Executor for CountingExecutor {
            fn subscribe(&self) -> Option<broadcast::Receiver<CdpEvent>> {
                None
            }

            async fn run_step(
                &self,
                _step: &Step,
                _row: &[String],
                _rules: &BTreeMap<usize, EmptyCellRule>,
                _pre_subscribed: Option<broadcast::Receiver<CdpEvent>>,
            ) -> Result<StepOutcome, PlaybackError> {
                let n = self.0.fetch_add(1, Ordering::Relaxed);
                if n == 0 {
                    Err(PlaybackError::Other("first attempt".to_owned()))
                } else {
                    Ok(StepOutcome::Executed)
                }
            }
        }

        let counter = Arc::new(AtomicUsize::new(0));
        let steps = vec![click_step()];
        let (mut engine, ctrl_tx, _event_rx) = test_engine(
            workflow_with_steps(steps),
            dataset(&["x"]),
            PlaybackSpeed::Run,
            0,
        );

        let exec = CountingExecutor(Arc::clone(&counter));
        let handle = tokio::spawn(async move { engine.run_with(exec).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        // First attempt fails → Retry.
        ctrl_tx
            .send(PlaybackControl::ErrorResponse(ErrorAction::RetryRow))
            .unwrap();

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result.rows_completed, 1);
        // Called twice: once for the original attempt, once for the retry.
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn stop_action_returns_stopped_error() {
        let steps = vec![click_step()];
        let (mut engine, ctrl_tx, _event_rx) = test_engine(
            workflow_with_steps(steps),
            dataset(&["x"]),
            PlaybackSpeed::Run,
            0,
        );

        let handle =
            tokio::spawn(async move { engine.run_with(ErrExecutor("fail".to_owned())).await });

        tokio::time::sleep(Duration::from_millis(20)).await;
        ctrl_tx
            .send(PlaybackControl::ErrorResponse(ErrorAction::Stop))
            .unwrap();

        let result = handle.await.unwrap();
        assert!(matches!(result, Err(PlaybackError::Stopped)));
    }

    // ── Speed-multiplier tests ────────────────────────────────────────────

    #[tokio::test]
    async fn set_speed_multiplier_applied_in_pending_controls() {
        // Engine runs at Run speed (no gates). Pre-load a SetSpeedMultiplier
        // so apply_pending_controls picks it up between steps, then verify
        // the multiplier is reflected via a SpeedMultiplierChanged event.
        let steps = vec![click_step()];
        let (mut engine, ctrl_tx, mut event_rx) = test_engine(
            workflow_with_steps(steps),
            dataset(&["x"]),
            PlaybackSpeed::Run,
            0,
        );

        // Clamp test: 8.0 should be stored as 4.0.
        ctrl_tx
            .send(PlaybackControl::SetSpeedMultiplier(8.0))
            .unwrap();

        engine.run_with(OkExecutor).await.unwrap();

        let events = drain_events(&mut event_rx);
        let changed: Vec<f64> = events
            .iter()
            .filter_map(|e| {
                if let PlaybackEvent::SpeedMultiplierChanged(m) = e {
                    Some(*m)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(changed, vec![4.0], "clamped multiplier should be 4.0");
        #[allow(clippy::float_cmp)] // 4.0 is exact in IEEE 754
        {
            assert_eq!(engine.speed_multiplier, 4.0);
        }
    }

    #[tokio::test]
    async fn set_speed_multiplier_clamps_below() {
        let steps = vec![click_step()];
        let (mut engine, ctrl_tx, mut event_rx) = test_engine(
            workflow_with_steps(steps),
            dataset(&["x"]),
            PlaybackSpeed::Run,
            0,
        );

        ctrl_tx
            .send(PlaybackControl::SetSpeedMultiplier(0.1))
            .unwrap();

        engine.run_with(OkExecutor).await.unwrap();

        let events = drain_events(&mut event_rx);
        let changed: Vec<f64> = events
            .iter()
            .filter_map(|e| {
                if let PlaybackEvent::SpeedMultiplierChanged(m) = e {
                    Some(*m)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(changed, vec![0.25], "clamped multiplier should be 0.25");
        #[allow(clippy::float_cmp)] // 0.25 is exact in IEEE 754
        {
            assert_eq!(engine.speed_multiplier, 0.25);
        }
    }

    // ── Column-count cross-check ──────────────────────────────────────────

    #[tokio::test]
    async fn run_with_rejects_insufficient_columns() {
        // Workflow expects 3 columns but data only has 1.
        let workflow = Workflow::new(
            3,
            vec![],
            vec![],
            Duration::ZERO,
            vec![None, None, None],
            BTreeMap::new(),
            None,
        );
        let ds = dataset(&["x", "y"]);

        let (mut engine, _ctrl, _event_rx) = test_engine(workflow, ds, PlaybackSpeed::Run, 0);

        let result = engine.run_with(OkExecutor).await;
        assert!(matches!(result, Err(PlaybackError::Other(_))));
    }

    // ── Workflow round-trip → playback integration ────────────────────────

    #[tokio::test]
    async fn workflow_roundtrip_and_playback() {
        // Build a two-step workflow with a column binding.
        let mut rules = BTreeMap::new();
        rules.insert(1usize, EmptyCellRule::Skip);
        let workflow = Workflow::new(
            2,
            vec![
                Step::Click {
                    selector: Selector {
                        strategies: vec![Resolution::Id {
                            id: "btn".to_owned(),
                        }],
                        tag: "BUTTON".to_owned(),
                    },
                    navigation: None,
                },
                Step::Type {
                    selector: Selector {
                        strategies: vec![],
                        tag: "INPUT".to_owned(),
                    },
                    source: ValueSource::Column { index: 0 },
                    navigation: None,
                },
            ],
            vec![Duration::ZERO, Duration::ZERO],
            Duration::ZERO,
            vec![None, Some(1)],
            rules,
            None,
        );

        // Serialize → deserialize round-trip.
        let json = workflow.to_json().unwrap();
        let loaded = Workflow::from_json(&json).unwrap();
        assert_eq!(loaded, workflow);

        // Run with OkExecutor against 2-column data.
        let ds = crate::data::from_delimited_str(
            "name\tage\nAlice\t30\nBob\t25\n",
            crate::data::Delimiter::Tab,
            true,
        )
        .expect("test dataset");

        let (mut engine, _ctrl, mut event_rx) = test_engine(loaded, ds, PlaybackSpeed::Run, 0);

        let result = engine.run_with(OkExecutor).await.unwrap();
        assert_eq!(result.rows_completed, 2);
        assert_eq!(result.rows_skipped, 0);

        let events = drain_events(&mut event_rx);
        let finished = events.iter().find_map(|e| {
            if let PlaybackEvent::Finished {
                rows_completed,
                rows_skipped,
            } = e
            {
                Some((*rows_completed, *rows_skipped))
            } else {
                None
            }
        });
        assert_eq!(finished, Some((2, 0)));
    }
}
