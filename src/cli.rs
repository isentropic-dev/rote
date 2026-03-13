use std::path::PathBuf;

use clap::Parser;

/// Record once, replay forever.
/// Automate repetitive web data entry by watching a human do it once.
#[derive(Debug, Parser)]
#[command(version)]
pub struct Args {
    /// Read data from the system clipboard.
    #[arg(long)]
    pub clipboard: bool,

    /// Read data from a file (TSV or CSV).
    #[arg(long, value_name = "FILE")]
    pub data: Option<PathBuf>,

    /// Load a saved workflow file.
    #[arg(long, value_name = "FILE")]
    pub workflow: Option<PathBuf>,
}
