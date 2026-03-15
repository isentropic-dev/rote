mod cdp;
mod cli;
mod data;
#[allow(dead_code, unused_imports)]
mod playback;
mod training;
mod tui;
#[allow(dead_code)]
mod workflow;

use clap::Parser;

use cli::Args;
use data::Delimiter;

fn main() {
    let args = Args::parse();

    // Headers are always required.
    let maybe_dataset = if args.clipboard {
        match data::from_clipboard(true) {
            Ok(ds) => Some(ds),
            Err(e) => {
                eprintln!("Failed to read clipboard: {e}");
                None
            }
        }
    } else if let Some(ref path) = args.data {
        match data::from_file(path, Delimiter::Tab, true) {
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

    let outcome = rt.block_on(tui::run(dataset, args.url));

    match outcome {
        Ok(tui::Outcome::Quit) => {
            println!("Goodbye.");
        }
        Err(e) => {
            eprintln!("TUI error: {e}");
        }
    }
}
