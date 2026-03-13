// Data source configuration for workflow serialization.

use serde::{Deserialize, Serialize};

/// How fields are delimited in the data source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delimiter {
    Tab,
    Comma,
}

impl Delimiter {
    /// The byte value used by the csv crate.
    pub fn as_byte(self) -> u8 {
        match self {
            Delimiter::Tab => b'\t',
            Delimiter::Comma => b',',
        }
    }
}

/// Describes where data came from, stored in workflow files
/// so playback knows how to load data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum DataSourceConfig {
    Clipboard {
        has_headers: bool,
    },
    File {
        path: String,
        delimiter: Delimiter,
        has_headers: bool,
    },
}

impl DataSourceConfig {
    /// Config for clipboard-sourced data.
    pub fn clipboard(has_headers: bool) -> Self {
        DataSourceConfig::Clipboard { has_headers }
    }

    /// Config for file-sourced data.
    pub fn file(path: impl Into<String>, delimiter: Delimiter, has_headers: bool) -> Self {
        DataSourceConfig::File {
            path: path.into(),
            delimiter,
            has_headers,
        }
    }
}
