use std::path::PathBuf;

use clap::Parser;

/// Record once, replay the rest.
/// Automate web form data entry by example.
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

    /// Navigate the browser to this URL and start training immediately.
    #[arg(long, value_name = "URL")]
    pub url: Option<String>,
}
