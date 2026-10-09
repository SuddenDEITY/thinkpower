mod cli;
mod client;
mod gui;

use clap::Parser;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = cli::Cli::parse();
    match args.command {
        None | Some(cli::Command::Gui) => match gui::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
        Some(cmd) => cli::run(cmd),
    }
}
