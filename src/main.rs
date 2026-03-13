mod cdp;
mod cli;
mod data;
mod training;
mod tui;
mod workflow;

use clap::Parser;

use cli::Args;

fn main() {
    let args = Args::parse();

    println!("rote — record once, replay forever.");
    println!();

    if args.clipboard {
        println!("  --clipboard: enabled");
    }
    if let Some(ref path) = args.data {
        println!("  --data: {}", path.display());
    }
    if let Some(ref path) = args.workflow {
        println!("  --workflow: {}", path.display());
    }

    if !args.clipboard && args.data.is_none() && args.workflow.is_none() {
        println!("  (no flags — would launch TUI with data source prompt)");
    }
}
