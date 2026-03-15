// Workflow serialization and types.
//
// These types are shared between the training recorder and future playback.
// They serialize to JSON workflow files.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;
use std::{fs, io};

use serde::{Deserialize, Serialize};

// ─── Duration serde helpers ───────────────────────────────────────────────────

/// Serialize/deserialize a [`Duration`] as milliseconds (u64).
mod serde_duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        // Durations exceeding ~585M years would truncate, but that's not a
        // realistic step delay.
        #[allow(clippy::cast_possible_truncation)]
        s.serialize_u64(d.as_millis() as u64)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_millis)
    }
}

/// Serialize/deserialize a `Vec<Duration>` as a JSON array of millisecond integers.
mod serde_duration_millis_vec {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &[Duration], s: S) -> Result<S::Ok, S::Error> {
        // Durations exceeding ~585M years would truncate, not a realistic step delay.
        #[allow(clippy::cast_possible_truncation)]
        s.collect_seq(v.iter().map(|d| d.as_millis() as u64))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Duration>, D::Error> {
        Vec::<u64>::deserialize(d)
            .map(|v| v.into_iter().map(Duration::from_millis).collect())
    }
}

use crate::data::DataSourceConfig;

/// How an element is found in the DOM.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Resolution {
    /// Match by HTML `id` attribute.
    Id { id: String },
    /// Match by CSS selector.
    Css { selector: String },
    /// Match by `XPath` expression.
    XPath { path: String },
    /// Match by visible text content.
    TextContent { text: String },
}

/// Multi-strategy selector for a DOM element.
///
/// Contains one or more resolution strategies. During playback, strategies
/// are tried in order until one succeeds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Selector {
    /// Resolution strategies, tried in order.
    pub strategies: Vec<Resolution>,
    /// The HTML tag name of the target element (e.g. "INPUT", "BUTTON").
    pub tag: String,
}

/// Where a type step gets its value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ValueSource {
    /// Value comes from a data column.
    Column { index: usize },
    /// Static literal value, same for every row.
    Literal { value: String },
}

/// A single recorded action in the workflow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "action", rename_all = "camelCase")]
pub enum Step {
    /// Click on an element.
    Click { selector: Selector },
    /// Type a value into an element.
    Type {
        selector: Selector,
        source: ValueSource,
    },
    /// Wait for a browser navigation to complete.
    ///
    /// This step does not trigger navigation itself; it waits for a navigation
    /// that was triggered by the previous step (typically a form submit or
    /// click). The engine pre-subscribes to CDP events before executing the
    /// previous step to avoid a race condition.
    WaitForNavigation,
}

/// What to do when a cell is blank for a bound column.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum EmptyCellRule {
    /// Skip the step entirely.
    Skip,
    /// Clear the field.
    Clear,
    /// Use a default value.
    Default { value: String },
}

/// Playback speed levels.
///
/// Controls the pacing granularity during playback. Step is the most granular
/// (pause after each field), Walk pauses between rows, Run goes continuously.
/// Pause (Space) can halt at any speed — there is no separate "manual" level.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PlaybackSpeed {
    /// Pause after each field fill (Type step). Clicks and navigations auto-advance.
    #[default]
    Step,
    /// Pause at end of each row. Steps within a row auto-advance with delay.
    Walk,
    /// Fully automatic. No gates, minimal delay.
    Run,
}

/// Current workflow format version.
const FORMAT_VERSION: u32 = 2;

/// A complete workflow: the artifact that bridges sessions.
///
/// Contains everything needed to replay a recorded data entry sequence.
/// Serialized to JSON for storage, sharing, and version control.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Workflow {
    /// Format version for forward compatibility.
    pub version: u32,
    /// Number of data columns this workflow expects.
    pub column_count: usize,
    /// The recorded sequence of actions.
    pub steps: Vec<Step>,
    /// Per-step delay captured during training.
    ///
    /// `step_delays[i]` is the elapsed time before step `i` was recorded
    /// (relative to the previous step, or zero for the first step).
    /// During playback, the engine sleeps this duration after each step.
    /// Always has exactly `steps.len()` entries.
    /// Defaults allow v1 files to deserialize so `validate()` can report a
    /// clean "unsupported workflow version" error instead of a serde field error.
    #[serde(default, with = "serde_duration_millis_vec")]
    pub step_delays: Vec<Duration>,
    /// Delay from the last step to when the row was finalized during training.
    ///
    /// Used during playback to pace the transition between rows.
    #[serde(default, with = "serde_duration_millis")]
    pub row_end_delay: Duration,
    /// Per-column binding: `column_bindings[col]` is `Some(step_index)` when
    /// that column is bound to a [`Step::Type`] step. Always has exactly
    /// `column_count` entries; `None` means the column is not yet bound.
    pub column_bindings: Vec<Option<usize>>,
    /// Per-column rules for handling empty cells.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub empty_cell_rules: BTreeMap<usize, EmptyCellRule>,
    /// How the data was originally loaded (so playback can reload it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_source: Option<DataSourceConfig>,
}

/// Errors from workflow serialization and validation.
#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unsupported workflow version: {found} (max supported: {max})")]
    UnsupportedVersion { found: u32, max: u32 },

    #[error("column binding references step {step_index}, but only {step_count} steps exist")]
    InvalidBinding {
        step_index: usize,
        step_count: usize,
    },

    #[error("empty cell rule references column {column}, but only {column_count} columns exist")]
    InvalidEmptyCellRule { column: usize, column_count: usize },

    #[error("column_bindings has {found} entries but column_count is {expected}")]
    ColumnBindingLengthMismatch { expected: usize, found: usize },

    #[error("step_delays has {found} entries but steps has {expected}")]
    StepDelayLengthMismatch { expected: usize, found: usize },

    #[error("delay value {millis}ms exceeds maximum ({max}ms)")]
    DelayTooLarge { millis: u64, max: u64 },

    #[error("column {column} is bound to step {step_index}, which is not a Type step")]
    BindingNotTypeStep { column: usize, step_index: usize },
}

impl Workflow {
    /// Create a new workflow with the current format version.
    #[must_use]
    pub fn new(
        column_count: usize,
        steps: Vec<Step>,
        step_delays: Vec<Duration>,
        row_end_delay: Duration,
        column_bindings: Vec<Option<usize>>,
        empty_cell_rules: BTreeMap<usize, EmptyCellRule>,
        data_source: Option<DataSourceConfig>,
    ) -> Self {
        Self {
            version: FORMAT_VERSION,
            column_count,
            steps,
            step_delays,
            row_end_delay,
            column_bindings,
            empty_cell_rules,
            data_source,
        }
    }

    /// Serialize to pretty-printed JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization fails.
    pub fn to_json(&self) -> Result<String, WorkflowError> {
        serde_json::to_string_pretty(self).map_err(WorkflowError::from)
    }

    /// Deserialize from JSON, then validate.
    ///
    /// # Errors
    ///
    /// Returns an error if the JSON is invalid, the version is unsupported,
    /// or the internal references are inconsistent.
    pub fn from_json(json: &str) -> Result<Self, WorkflowError> {
        let workflow: Self = serde_json::from_str(json)?;
        workflow.validate()?;
        Ok(workflow)
    }

    /// Save to a file.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or file writing fails.
    pub fn save(&self, path: &Path) -> Result<(), WorkflowError> {
        let json = self.to_json()?;
        fs::write(path, json)?;
        Ok(())
    }

    /// Load from a file, then validate.
    ///
    /// # Errors
    ///
    /// Returns an error if reading, parsing, or validation fails.
    pub fn load(path: &Path) -> Result<Self, WorkflowError> {
        let json = fs::read_to_string(path)?;
        Self::from_json(&json)
    }

    /// Validate internal consistency.
    ///
    /// # Errors
    ///
    /// Returns an error if the version is unsupported or references are invalid.
    pub fn validate(&self) -> Result<(), WorkflowError> {
        if self.version > FORMAT_VERSION {
            return Err(WorkflowError::UnsupportedVersion {
                found: self.version,
                max: FORMAT_VERSION,
            });
        }

        if self.column_bindings.len() != self.column_count {
            return Err(WorkflowError::ColumnBindingLengthMismatch {
                expected: self.column_count,
                found: self.column_bindings.len(),
            });
        }

        if self.step_delays.len() != self.steps.len() {
            return Err(WorkflowError::StepDelayLengthMismatch {
                expected: self.steps.len(),
                found: self.step_delays.len(),
            });
        }

        // Reject absurdly large delays (e.g. from crafted workflow files).
        // 30s is generous — real training delays are sub-second.
        let max_delay = Duration::from_secs(30);
        let max_ms: u64 = 30_000;
        for delay in &self.step_delays {
            if *delay > max_delay {
                return Err(WorkflowError::DelayTooLarge {
                    millis: delay.as_millis().try_into().unwrap_or(u64::MAX),
                    max: max_ms,
                });
            }
        }
        if self.row_end_delay > max_delay {
            return Err(WorkflowError::DelayTooLarge {
                millis: self.row_end_delay.as_millis().try_into().unwrap_or(u64::MAX),
                max: max_ms,
            });
        }

        for (col, binding) in self.column_bindings.iter().enumerate() {
            if let Some(step_index) = binding {
                if *step_index >= self.steps.len() {
                    return Err(WorkflowError::InvalidBinding {
                        step_index: *step_index,
                        step_count: self.steps.len(),
                    });
                }
                if !matches!(self.steps[*step_index], Step::Type { .. }) {
                    return Err(WorkflowError::BindingNotTypeStep {
                        column: col,
                        step_index: *step_index,
                    });
                }
            }
        }

        for &column in self.empty_cell_rules.keys() {
            if column >= self.column_count {
                return Err(WorkflowError::InvalidEmptyCellRule {
                    column,
                    column_count: self.column_count,
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolution_serialization() {
        let r = Resolution::Id {
            id: "foo".to_owned(),
        };
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains(r#""type":"id""#));
        let back: Resolution = serde_json::from_str(&json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn step_click_serialization() {
        let step = Step::Click {
            selector: Selector {
                strategies: vec![Resolution::Id {
                    id: "btn".to_owned(),
                }],
                tag: "BUTTON".to_owned(),
            },
        };
        let json = serde_json::to_string_pretty(&step).unwrap();
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back, step);
    }

    #[test]
    fn step_type_column_serialization() {
        let step = Step::Type {
            selector: Selector {
                strategies: vec![],
                tag: "INPUT".to_owned(),
            },
            source: ValueSource::Column { index: 2 },
        };
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains(r#""action":"type""#));
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back, step);
    }

    #[test]
    fn step_wait_for_navigation_serialization() {
        let step = Step::WaitForNavigation;
        let json = serde_json::to_string(&step).unwrap();
        assert!(json.contains(r#""action":"waitForNavigation""#));
        let back: Step = serde_json::from_str(&json).unwrap();
        assert_eq!(back, step);
    }

    #[test]
    fn empty_cell_rule_serialization() {
        let rules = vec![
            EmptyCellRule::Skip,
            EmptyCellRule::Clear,
            EmptyCellRule::Default {
                value: "N/A".to_owned(),
            },
        ];
        for rule in rules {
            let json = serde_json::to_string(&rule).unwrap();
            let back: EmptyCellRule = serde_json::from_str(&json).unwrap();
            assert_eq!(back, rule);
        }
    }

    #[test]
    fn playback_speed_default() {
        assert_eq!(PlaybackSpeed::default(), PlaybackSpeed::Step);
    }

    #[test]
    fn playback_speed_serialization() {
        let speed = PlaybackSpeed::Run;
        let json = serde_json::to_string(&speed).unwrap();
        assert_eq!(json, r#""run""#);
        let back: PlaybackSpeed = serde_json::from_str(&json).unwrap();
        assert_eq!(back, speed);
    }

    fn sample_workflow() -> Workflow {
        let mut rules = BTreeMap::new();
        rules.insert(1, EmptyCellRule::Skip);

        Workflow::new(
            3,
            vec![
                Step::WaitForNavigation,
                Step::Click {
                    selector: Selector {
                        strategies: vec![Resolution::Id {
                            id: "name-field".to_owned(),
                        }],
                        tag: "INPUT".to_owned(),
                    },
                },
                Step::Type {
                    selector: Selector {
                        strategies: vec![
                            Resolution::Id {
                                id: "name-field".to_owned(),
                            },
                            Resolution::Css {
                                selector: "#name-field".to_owned(),
                            },
                        ],
                        tag: "INPUT".to_owned(),
                    },
                    source: ValueSource::Column { index: 0 },
                },
                Step::Type {
                    selector: Selector {
                        strategies: vec![Resolution::Css {
                            selector: "#age-field".to_owned(),
                        }],
                        tag: "INPUT".to_owned(),
                    },
                    source: ValueSource::Column { index: 1 },
                },
                Step::Click {
                    selector: Selector {
                        strategies: vec![Resolution::TextContent {
                            text: "Submit".to_owned(),
                        }],
                        tag: "BUTTON".to_owned(),
                    },
                },
            ],
            vec![
                Duration::ZERO,
                Duration::from_millis(500),
                Duration::from_millis(300),
                Duration::from_millis(400),
                Duration::from_millis(200),
            ],
            Duration::from_millis(150),
            vec![Some(2), Some(3), None],
            rules,
            Some(DataSourceConfig::file(
                "data.tsv",
                crate::data::Delimiter::Tab,
                true,
            )),
        )
    }

    #[test]
    fn workflow_round_trip() {
        let workflow = sample_workflow();
        let json = workflow.to_json().unwrap();
        let back = Workflow::from_json(&json).unwrap();
        assert_eq!(back, workflow);
    }

    #[test]
    fn workflow_has_version() {
        let workflow = sample_workflow();
        let json = workflow.to_json().unwrap();
        assert!(json.contains(r#""version": 2"#));
    }

    #[test]
    fn workflow_omits_empty_optional_fields() {
        let workflow = Workflow::new(
            1,
            vec![],
            vec![],
            Duration::ZERO,
            vec![None],
            BTreeMap::new(),
            None,
        );
        let json = workflow.to_json().unwrap();
        assert!(!json.contains("emptyCellRules"));
        assert!(!json.contains("dataSource"));
    }

    #[test]
    fn workflow_unsupported_version() {
        let mut workflow = sample_workflow();
        workflow.version = 999;
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("unsupported workflow version"));
    }

    #[test]
    fn workflow_invalid_binding() {
        let workflow = Workflow {
            version: 2,
            column_count: 1,
            steps: vec![],
            step_delays: vec![],
            row_end_delay: Duration::ZERO,
            column_bindings: vec![Some(5)],
            empty_cell_rules: BTreeMap::new(),
            data_source: None,
        };
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(err.to_string().contains("column binding references step 5"));
    }

    #[test]
    fn workflow_invalid_empty_cell_rule() {
        let mut rules = BTreeMap::new();
        rules.insert(10, EmptyCellRule::Clear);
        let workflow = Workflow {
            version: 2,
            column_count: 2,
            steps: vec![],
            step_delays: vec![],
            row_end_delay: Duration::ZERO,
            column_bindings: vec![None, None],
            empty_cell_rules: rules,
            data_source: None,
        };
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(
            err.to_string()
                .contains("empty cell rule references column 10")
        );
    }

    #[test]
    fn workflow_file_round_trip() {
        let workflow = sample_workflow();
        let dir = std::env::temp_dir().join("rote-test-workflow");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.json");

        workflow.save(&path).unwrap();
        let loaded = Workflow::load(&path).unwrap();
        assert_eq!(loaded, workflow);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workflow_json_is_human_readable() {
        let workflow = sample_workflow();
        let json = workflow.to_json().unwrap();
        // Pretty-printed: should have newlines and indentation.
        assert!(json.contains('\n'));
        assert!(json.contains("  "));
        // Key fields should be camelCase.
        assert!(json.contains("columnCount"));
        assert!(json.contains("stepDelays"));
        assert!(json.contains("rowEndDelay"));
        assert!(json.contains("columnBindings"));
        assert!(json.contains("emptyCellRules"));
        assert!(json.contains("dataSource"));
    }

    #[test]
    fn workflow_column_binding_length_mismatch() {
        let workflow = Workflow {
            version: 2,
            column_count: 3,
            steps: vec![],
            step_delays: vec![],
            row_end_delay: Duration::ZERO,
            column_bindings: vec![None, None], // length 2, but column_count is 3
            empty_cell_rules: BTreeMap::new(),
            data_source: None,
        };
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(
            err.to_string().contains("column_bindings has 2 entries"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn workflow_binding_not_type_step() {
        let click = Step::Click {
            selector: Selector {
                strategies: vec![],
                tag: "BUTTON".to_owned(),
            },
        };
        let workflow = Workflow {
            version: 2,
            column_count: 1,
            steps: vec![click],
            step_delays: vec![Duration::ZERO],
            row_end_delay: Duration::ZERO,
            column_bindings: vec![Some(0)], // bound to a Click step, not Type
            empty_cell_rules: BTreeMap::new(),
            data_source: None,
        };
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(
            err.to_string().contains("not a Type step"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn workflow_delay_length_mismatch() {
        let workflow = Workflow {
            version: 2,
            column_count: 0,
            steps: vec![Step::WaitForNavigation],
            step_delays: vec![], // length 0, but steps has 1
            row_end_delay: Duration::ZERO,
            column_bindings: vec![],
            empty_cell_rules: BTreeMap::new(),
            data_source: None,
        };
        let json = serde_json::to_string(&workflow).unwrap();
        let err = Workflow::from_json(&json).unwrap_err();
        assert!(
            err.to_string().contains("step_delays has 0 entries"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn workflow_delays_round_trip() {
        let workflow = sample_workflow();
        let json = workflow.to_json().unwrap();
        // Delays should be serialized as millisecond integers.
        assert!(json.contains("stepDelays"));
        assert!(json.contains("rowEndDelay"));
        let back = Workflow::from_json(&json).unwrap();
        assert_eq!(back.step_delays, workflow.step_delays);
        assert_eq!(back.row_end_delay, workflow.row_end_delay);
    }
}
