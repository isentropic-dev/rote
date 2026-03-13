// Tabular data parsing via the `csv` crate.

use super::DataError;

/// A rectangular table of string values with an optional header row.
#[derive(Debug, Clone)]
pub struct DataSet {
    headers: Option<Vec<String>>,
    rows: Vec<Vec<String>>,
    column_count: usize,
}

impl DataSet {
    /// Number of data rows (excludes headers).
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Number of columns.
    pub fn column_count(&self) -> usize {
        self.column_count
    }

    /// Column headers, if present.
    pub fn headers(&self) -> Option<&[String]> {
        self.headers.as_deref()
    }

    /// Get a data row by index.
    pub fn row(&self, index: usize) -> Option<&[String]> {
        self.rows.get(index).map(Vec::as_slice)
    }

    /// Iterator over all data rows.
    pub fn rows(&self) -> &[Vec<String>] {
        &self.rows
    }
}

/// Parse delimited text into a `DataSet`.
///
/// The `delimiter` byte controls splitting (e.g. `b'\t'` for TSV, `b','` for CSV).
/// When `has_headers` is true, the first row becomes column headers
/// and is excluded from the data rows.
pub fn parse(text: &str, delimiter: u8, has_headers: bool) -> Result<DataSet, DataError> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());

    let mut all_rows: Vec<Vec<String>> = Vec::new();

    for result in reader.records() {
        let record = result?;
        let row: Vec<String> = record.iter().map(String::from).collect();

        // Skip rows that are entirely empty or whitespace.
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }

        all_rows.push(row);
    }

    if all_rows.is_empty() {
        return Err(DataError::Empty);
    }

    let (headers, rows) = if has_headers {
        let headers = all_rows.remove(0);
        if all_rows.is_empty() {
            return Err(DataError::Empty);
        }
        (Some(headers), all_rows)
    } else {
        (None, all_rows)
    };

    let column_count = headers.as_ref().map_or(rows[0].len(), Vec::len);

    // Validate consistent column counts.
    for (i, row) in rows.iter().enumerate() {
        if row.len() != column_count {
            return Err(DataError::InconsistentColumns {
                // 1-indexed row number within data rows.
                row: i + 1,
                expected: column_count,
                found: row.len(),
            });
        }
    }

    Ok(DataSet {
        headers,
        rows,
        column_count,
    })
}
