// Tabular data loading and parsing.

mod parse;
mod source;

pub use parse::DataSet;
pub use source::{DataSourceConfig, Delimiter};

use std::{fs, io, path::Path};

use thiserror::Error;

/// Errors that can occur when loading or parsing tabular data.
#[derive(Debug, Error)]
pub enum DataError {
    #[error("clipboard error: {0}")]
    Clipboard(String),

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("CSV parse error: {0}")]
    Csv(#[from] csv::Error),

    #[error("data is empty")]
    Empty,

    #[error("inconsistent column count: row {row} has {found} columns, expected {expected}")]
    InconsistentColumns {
        row: usize,
        expected: usize,
        found: usize,
    },
}

/// Read tab-separated data from the system clipboard.
///
/// This is what spreadsheet applications put on the clipboard
/// when you copy a selection.
pub fn from_clipboard(has_headers: bool) -> Result<DataSet, DataError> {
    let text = clipboard_text()?;
    from_delimited_str(&text, Delimiter::Tab, has_headers)
}

/// Read delimited data from a file.
pub fn from_file(
    path: &Path,
    delimiter: Delimiter,
    has_headers: bool,
) -> Result<DataSet, DataError> {
    let text = fs::read_to_string(path)?;
    from_delimited_str(&text, delimiter, has_headers)
}

/// Parse delimited text into a `DataSet`.
pub fn from_delimited_str(
    text: &str,
    delimiter: Delimiter,
    has_headers: bool,
) -> Result<DataSet, DataError> {
    parse::parse(text, delimiter.as_byte(), has_headers)
}

/// Read text from the system clipboard.
///
/// Separated out so tests can avoid clipboard access.
fn clipboard_text() -> Result<String, DataError> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|e| DataError::Clipboard(e.to_string()))?;
    clipboard
        .get_text()
        .map_err(|e| DataError::Clipboard(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsv_round_trip() {
        let tsv = "name\tage\tcity\nAlice\t30\tPortland\nBob\t25\tSeattle\n";
        let ds = from_delimited_str(tsv, Delimiter::Tab, true).unwrap();
        assert_eq!(ds.headers().unwrap(), &["name", "age", "city"]);
        assert_eq!(ds.row_count(), 2);
        assert_eq!(ds.column_count(), 3);
        let expected: &[&str] = &["Alice", "30", "Portland"];
        assert_eq!(ds.row(0).unwrap(), expected);
    }

    #[test]
    fn csv_round_trip() {
        let csv_text = "a,b,c\n1,2,3\n4,5,6\n";
        let ds = from_delimited_str(csv_text, Delimiter::Comma, true).unwrap();
        assert_eq!(ds.headers().unwrap(), &["a", "b", "c"]);
        assert_eq!(ds.row_count(), 2);
    }

    #[test]
    fn no_headers() {
        let tsv = "Alice\t30\nBob\t25\n";
        let ds = from_delimited_str(tsv, Delimiter::Tab, false).unwrap();
        assert!(ds.headers().is_none());
        assert_eq!(ds.row_count(), 2);
        assert_eq!(ds.column_count(), 2);
    }

    #[test]
    fn empty_data_is_error() {
        let result = from_delimited_str("", Delimiter::Tab, false);
        assert!(matches!(result, Err(DataError::Empty)));
    }

    #[test]
    fn headers_only_is_error() {
        let result = from_delimited_str("a\tb\tc\n", Delimiter::Tab, true);
        assert!(matches!(result, Err(DataError::Empty)));
    }

    #[test]
    fn inconsistent_columns_is_error() {
        let tsv = "a\tb\tc\n1\t2\n";
        let result = from_delimited_str(tsv, Delimiter::Tab, true);
        assert!(matches!(
            result,
            Err(DataError::InconsistentColumns {
                row: 1,
                expected: 3,
                found: 2
            })
        ));
    }

    #[test]
    fn empty_cells_preserved() {
        let tsv = "a\t\tc\n\tb\t\n";
        let ds = from_delimited_str(tsv, Delimiter::Tab, false).unwrap();
        assert_eq!(ds.row(0).unwrap(), &["a", "", "c"]);
        assert_eq!(ds.row(1).unwrap(), &["", "b", ""]);
    }

    #[test]
    fn single_column() {
        let tsv = "header\nval1\nval2\n";
        let ds = from_delimited_str(tsv, Delimiter::Tab, true).unwrap();
        assert_eq!(ds.column_count(), 1);
        assert_eq!(ds.row_count(), 2);
        assert_eq!(ds.headers().unwrap(), &["header"]);
    }

    #[test]
    fn single_row_no_headers() {
        let tsv = "a\tb\tc\n";
        let ds = from_delimited_str(tsv, Delimiter::Tab, false).unwrap();
        assert_eq!(ds.row_count(), 1);
        assert_eq!(ds.column_count(), 3);
    }

    #[test]
    fn single_row_with_headers() {
        let tsv = "h1\th2\nval1\tval2\n";
        let ds = from_delimited_str(tsv, Delimiter::Tab, true).unwrap();
        assert_eq!(ds.row_count(), 1);
        assert_eq!(ds.headers().unwrap(), &["h1", "h2"]);
    }

    #[test]
    fn csv_with_quoting() {
        let text = "name,note\n\"Smith, John\",\"said \"\"hello\"\"\"\n";
        let ds = from_delimited_str(text, Delimiter::Comma, true).unwrap();
        assert_eq!(ds.row(0).unwrap(), &["Smith, John", "said \"hello\""]);
    }

    #[test]
    fn trailing_newlines_ignored() {
        let tsv = "a\tb\n1\t2\n\n\n";
        let ds = from_delimited_str(tsv, Delimiter::Tab, true).unwrap();
        assert_eq!(ds.row_count(), 1);
    }

    #[test]
    fn source_config_serialization() {
        let config = DataSourceConfig::clipboard(true);
        let json = serde_json::to_string(&config).unwrap();
        let back: DataSourceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config);

        let config = DataSourceConfig::file("data.csv", Delimiter::Comma, false);
        let json = serde_json::to_string(&config).unwrap();
        let back: DataSourceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back, config);
    }

    #[test]
    fn whitespace_only_is_empty() {
        let result = from_delimited_str("  \n\t\n", Delimiter::Tab, false);
        assert!(matches!(result, Err(DataError::Empty)));
    }
}
