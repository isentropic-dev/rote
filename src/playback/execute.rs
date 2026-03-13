// Step execution: perform individual workflow steps against the browser.

use std::collections::HashMap;

use tokio::sync::broadcast::error::RecvError;
use tokio::time::Duration;

use crate::cdp::Browser;
use crate::workflow::{EmptyCellRule, Step, ValueSource};

use super::{resolve, PlaybackError};

/// How long to wait for a `Page.frameNavigated` event.
const NAVIGATE_TIMEOUT: Duration = Duration::from_secs(30);

// ─── Type-value resolution ─────────────────────────────────────────────────

/// The action to take for a [`Step::Type`] step.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TypeAction {
    /// Skip this step (the cell was empty and the rule says to skip).
    Skip,
    /// Type the given value (may be an empty string for a Clear action).
    Type(String),
}

/// Outcome of executing a single step.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StepOutcome {
    /// The step ran and its action was performed.
    Executed,
    /// The step was skipped (empty-cell rule applied).
    Skipped,
}

/// Determine the effective value to type (or skip) for a [`Step::Type`].
///
/// Returns [`TypeAction::Skip`] when the cell is empty and the rule says so.
/// Returns [`TypeAction::Type`] with the effective value otherwise.
#[must_use]
pub(crate) fn resolve_type_value(
    source: &ValueSource,
    row: &[String],
    rules: &HashMap<usize, EmptyCellRule>,
) -> TypeAction {
    match source {
        ValueSource::Literal { value } => TypeAction::Type(value.clone()),

        ValueSource::Column { index } => {
            let cell = row.get(*index).map_or("", String::as_str);
            if cell.is_empty() {
                match rules.get(index) {
                    Some(EmptyCellRule::Skip) => TypeAction::Skip,
                    Some(EmptyCellRule::Default { value }) => TypeAction::Type(value.clone()),
                    // No rule or Clear → type an empty string (clears the field).
                    None | Some(EmptyCellRule::Clear) => TypeAction::Type(String::new()),
                }
            } else {
                TypeAction::Type(cell.to_owned())
            }
        }
    }
}

// ─── Individual step executors ─────────────────────────────────────────────

/// Execute a [`Step::Click`] step.
///
/// # Errors
///
/// - [`PlaybackError::ElementNotFound`] — element could not be resolved.
/// - [`PlaybackError::Cdp`] — a CDP command failed.
async fn execute_click(browser: &Browser, step: &Step) -> Result<(), PlaybackError> {
    let Step::Click { selector } = step else {
        return Err(PlaybackError::Other("execute_click called with non-Click step".to_owned()));
    };
    resolve::resolve_element(browser, selector).await?;
    browser.evaluate("window.__roteTarget.click()").await?;
    Ok(())
}

/// Execute a [`Step::Type`] step.
///
/// # Errors
///
/// - [`PlaybackError::ElementNotFound`] — element could not be resolved.
/// - [`PlaybackError::Cdp`] — a CDP command failed.
async fn execute_type(
    browser: &Browser,
    step: &Step,
    row: &[String],
    rules: &HashMap<usize, EmptyCellRule>,
) -> Result<StepOutcome, PlaybackError> {
    let Step::Type { selector, source } = step else {
        return Err(PlaybackError::Other("execute_type called with non-Type step".to_owned()));
    };

    let action = resolve_type_value(source, row, rules);

    match action {
        TypeAction::Skip => return Ok(StepOutcome::Skipped),

        TypeAction::Type(value) => {
            resolve::resolve_element(browser, selector).await?;

            // Focus the element.
            browser.evaluate("window.__roteTarget.focus()").await?;

            // Clear any existing value and dispatch an `input` event.
            browser
                .evaluate(
                    "window.__roteTarget.value='';\
                     window.__roteTarget.dispatchEvent(\
                       new Event('input',{bubbles:true}))",
                )
                .await?;

            // Insert text — this triggers real keyboard-like events.
            browser
                .send(
                    "Input.insertText",
                    Some(serde_json::json!({ "text": value })),
                )
                .await?;
        }
    }

    Ok(StepOutcome::Executed)
}

/// Execute a [`Step::Navigate`] step.
///
/// Waits passively for the browser to fire a `Page.frameNavigated` event on
/// the main frame (triggered by the previous step, typically a form submit).
///
/// # Errors
///
/// - [`PlaybackError::NavigationTimeout`] — no navigation within 30 seconds.
/// - [`PlaybackError::Other`] — the CDP event channel closed.
async fn execute_navigate(browser: &Browser) -> Result<(), PlaybackError> {
    let mut events = browser.subscribe();

    tokio::time::timeout(NAVIGATE_TIMEOUT, async {
        loop {
            match events.recv().await {
                Ok(event) => {
                    if event.method == "Page.frameNavigated" {
                        // A main frame navigation has no `parentId` on the frame.
                        let is_main_frame = event
                            .params
                            .get("frame")
                            .and_then(|f| f.get("parentId"))
                            .is_none();
                        if is_main_frame {
                            return Ok(());
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // We fell behind; keep listening — we may still catch it.
                }
                Err(RecvError::Closed) => {
                    return Err(PlaybackError::Other(
                        "CDP event channel closed during navigation wait".to_owned(),
                    ));
                }
            }
        }
    })
    .await
    .map_err(|_| PlaybackError::NavigationTimeout)?
}

// ─── Public dispatcher ────────────────────────────────────────────────────

/// Execute a single workflow step.
///
/// Returns [`StepOutcome::Skipped`] when an empty-cell rule suppresses the
/// step; [`StepOutcome::Executed`] when the step ran normally.
///
/// # Errors
///
/// - [`PlaybackError::ElementNotFound`] — element not found within timeout.
/// - [`PlaybackError::NavigationTimeout`] — navigation did not complete in time.
/// - [`PlaybackError::Cdp`] — a CDP command failed.
/// - [`PlaybackError::Other`] — internal misuse or unexpected condition.
pub(crate) async fn execute_step(
    browser: &Browser,
    step: &Step,
    row: &[String],
    rules: &HashMap<usize, EmptyCellRule>,
) -> Result<StepOutcome, PlaybackError> {
    match step {
        Step::Click { .. } => {
            execute_click(browser, step).await?;
            Ok(StepOutcome::Executed)
        }
        Step::Type { .. } => execute_type(browser, step, row, rules).await,
        Step::Navigate { .. } => {
            execute_navigate(browser).await?;
            Ok(StepOutcome::Executed)
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{EmptyCellRule, ValueSource};

    fn make_rules(pairs: &[(usize, EmptyCellRule)]) -> HashMap<usize, EmptyCellRule> {
        pairs.iter().cloned().collect()
    }

    #[test]
    fn literal_value_always_types() {
        let source = ValueSource::Literal {
            value: "hello".to_owned(),
        };
        let row: Vec<String> = vec![];
        let rules = make_rules(&[]);
        assert_eq!(
            resolve_type_value(&source, &row, &rules),
            TypeAction::Type("hello".to_owned()),
        );
    }

    #[test]
    fn column_with_value_types_it() {
        let source = ValueSource::Column { index: 0 };
        let row = vec!["Alice".to_owned(), "30".to_owned()];
        let rules = make_rules(&[]);
        assert_eq!(
            resolve_type_value(&source, &row, &rules),
            TypeAction::Type("Alice".to_owned()),
        );
    }

    #[test]
    fn empty_column_no_rule_clears() {
        let source = ValueSource::Column { index: 1 };
        let row = vec!["Alice".to_owned(), String::new()];
        let rules = make_rules(&[]);
        assert_eq!(
            resolve_type_value(&source, &row, &rules),
            TypeAction::Type(String::new()),
        );
    }

    #[test]
    fn empty_column_skip_rule_skips() {
        let source = ValueSource::Column { index: 1 };
        let row = vec!["Alice".to_owned(), String::new()];
        let rules = make_rules(&[(1, EmptyCellRule::Skip)]);
        assert_eq!(resolve_type_value(&source, &row, &rules), TypeAction::Skip,);
    }

    #[test]
    fn empty_column_clear_rule_clears() {
        let source = ValueSource::Column { index: 0 };
        let row = vec![String::new()];
        let rules = make_rules(&[(0, EmptyCellRule::Clear)]);
        assert_eq!(
            resolve_type_value(&source, &row, &rules),
            TypeAction::Type(String::new()),
        );
    }

    #[test]
    fn empty_column_default_rule_uses_default() {
        let source = ValueSource::Column { index: 2 };
        let row = vec!["a".to_owned(), "b".to_owned(), String::new()];
        let rules = make_rules(&[(2, EmptyCellRule::Default { value: "N/A".to_owned() })]);
        assert_eq!(
            resolve_type_value(&source, &row, &rules),
            TypeAction::Type("N/A".to_owned()),
        );
    }

    #[test]
    fn out_of_bounds_column_treats_as_empty() {
        let source = ValueSource::Column { index: 99 };
        let row = vec!["x".to_owned()];
        let rules = make_rules(&[(99, EmptyCellRule::Default { value: "X".to_owned() })]);
        assert_eq!(
            resolve_type_value(&source, &row, &rules),
            TypeAction::Type("X".to_owned()),
        );
    }

    #[test]
    fn non_empty_column_ignores_rule() {
        let source = ValueSource::Column { index: 0 };
        let row = vec!["Bob".to_owned()];
        // Even with a Skip rule, a non-empty cell should type normally.
        let rules = make_rules(&[(0, EmptyCellRule::Skip)]);
        assert_eq!(
            resolve_type_value(&source, &row, &rules),
            TypeAction::Type("Bob".to_owned()),
        );
    }
}
