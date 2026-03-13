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

    println!("rote — record once, replay forever.");
    println!();

    // Load data from the specified source.
    let dataset = if args.clipboard {
        println!("Reading data from clipboard...");
        match data::from_clipboard(true) {
            Ok(ds) => Some(ds),
            Err(e) => {
                eprintln!("Failed to read clipboard: {e}");
                None
            }
        }
    } else if let Some(ref path) = args.data {
        println!("Reading data from {}...", path.display());
        match data::from_file(path, Delimiter::Tab, true) {
            Ok(ds) => Some(ds),
            Err(e) => {
                eprintln!("Failed to read file: {e}");
                None
            }
        }
    } else {
        println!("No data source specified. Use --clipboard or --data <file>.");
        println!("(TUI data source prompt not yet implemented.)");
        None
    };

    // Show what we loaded.
    if let Some(ref ds) = dataset {
        println!();
        println!(
            "Loaded {} rows × {} columns.",
            ds.row_count(),
            ds.column_count(),
        );
        if let Some(headers) = ds.headers() {
            println!("Headers: {}", headers.join(", "));
        }
        if let Some(first) = ds.row(0) {
            println!("First row: {}", first.join(" | "));
        }
    }

    // Launch browser if we have data.
    if dataset.is_some() {
        println!();
        println!("Launching browser...");

        let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
        rt.block_on(async {
            match cdp::Browser::launch().await {
                Ok(browser) => {
                    println!("Browser connected via CDP.");
                    println!("Navigate to your form, then press Ctrl+C to exit.");
                    println!();

                    // Keep the browser alive until the user interrupts.
                    tokio::signal::ctrl_c()
                        .await
                        .expect("failed to listen for Ctrl+C");

                    println!();
                    println!("Shutting down...");
                    drop(browser);
                }
                Err(e) => {
                    eprintln!("Failed to launch browser: {e}");
                }
            }
        });
    }
}
