//! Standalone, read-only terminal viewer for retained ASB replay evidence.

mod app;
mod artifacts;
mod diff;
#[cfg(test)]
mod test_support;

use clap::Parser;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(
    version,
    about = "Inspect ASB replay mismatches offline; never runs agents or tools"
)]
struct Args {
    /// Campaign directory, batch directory, or retained .mismatch.json file.
    #[arg(
        value_name = "PATH",
        required_unless_present = "latest",
        conflicts_with = "latest"
    )]
    path: Option<PathBuf>,
    /// Open the newest campaign in PROJECT/results/asb-executions.
    #[arg(long, value_name = "PROJECT")]
    latest: Option<PathBuf>,
    /// Initially select this replica, using its saved run record attribution.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=64))]
    replica: Option<u64>,
    /// Include matching fields and unchanged text context initially.
    #[arg(long)]
    all: bool,
    /// Print the selected case's differences and outcomes without a terminal UI.
    #[arg(long)]
    print: bool,
}

fn run(args: Args) -> Result<(), String> {
    let path = match args.latest {
        Some(project) => artifacts::latest_campaign(&project)?,
        None => args.path.expect("clap requires an input"),
    };
    let set = artifacts::ArtifactSet::open(&path)?;
    let mut app = app::App::new(set, args.replica, args.all)?;
    if args.print {
        app.print(&mut std::io::stdout().lock())
            .map_err(|error| error.to_string())
    } else {
        app::run(&mut app).map_err(|error| error.to_string())
    }
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("asb-replay-debug: {}", diff::visible(&error));
            ExitCode::FAILURE
        }
    }
}
