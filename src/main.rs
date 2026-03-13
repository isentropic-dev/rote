mod cdp;
mod cli;
mod data;
mod playback;
mod training;
mod tui;
mod workflow;

use clap::Parser;

use cli::Args;
use data::Delimiter;

fn main() {
    let args = Args::parse();

    // Load data raw (has_headers=false) — the TUI will ask the user.
    let maybe_dataset = if args.clipboard {
        match data::from_clipboard(false) {
            Ok(ds) => Some(ds),
            Err(e) => {
                eprintln!("Failed to read clipboard: {e}");
                None
            }
        }
    } else if let Some(ref path) = args.data {
        match data::from_file(path, Delimiter::Tab, false) {
            Ok(ds) => Some(ds),
            Err(e) => {
                eprintln!("Failed to read {}: {e}", path.display());
                None
            }
        }
    } else {
        eprintln!("No data source specified. Use --clipboard or --data <file>.");
        return;
    };

    let Some(dataset) = maybe_dataset else {
        return;
    };

    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");

    let outcome = rt.block_on(tui::run(dataset));

    match outcome {
        Ok(tui::Outcome::Quit) => {
            println!("Goodbye.");
        }
        Err(e) => {
            eprintln!("TUI error: {e}");
        }
    }
}
