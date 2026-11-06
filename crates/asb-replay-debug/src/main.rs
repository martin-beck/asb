//! Standalone, read-only terminal viewer for retained ASB replay evidence.

mod app;
mod artifacts;
mod cassette_browser;
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
        required_unless_present_any = ["latest", "cassette"],
        conflicts_with_all = ["latest", "cassette"]
    )]
    path: Option<PathBuf>,
    /// Open the newest campaign in PROJECT/results/asb-executions.
    #[arg(long, value_name = "PROJECT", conflicts_with = "cassette")]
    latest: Option<PathBuf>,
    /// Browse a recorded cassette bundle or legacy JSONL source offline.
    #[arg(long, value_name = "PATH")]
    cassette: Option<PathBuf>,
    /// Select a cassette track by its recorded ID.
    #[arg(long, requires = "cassette", conflicts_with = "replica", value_parser = clap::value_parser!(u64).range(1..))]
    track: Option<u64>,
    /// Inspect the selected track's native interaction stream.
    #[arg(long, requires = "cassette", conflicts_with = "artifact")]
    native: bool,
    /// Inspect a named auxiliary artifact belonging to the selected track.
    #[arg(long, requires = "cassette", value_name = "KIND")]
    artifact: Option<String>,
    /// Initially select this replica, using its saved run record attribution.
    #[arg(long, conflicts_with = "cassette", value_parser = clap::value_parser!(u64).range(1..=64))]
    replica: Option<u64>,
    /// Include matching fields and unchanged text context initially.
    #[arg(long, conflicts_with = "cassette")]
    all: bool,
    /// Print the selected case's differences and outcomes without a terminal UI.
    #[arg(long)]
    print: bool,
}

fn run(args: Args) -> Result<(), String> {
    if let Some(path) = &args.cassette {
        return cassette_browser::open(
            path,
            args.track,
            args.native,
            args.artifact.as_deref(),
            args.print,
        );
    }
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
