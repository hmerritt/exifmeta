use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = exifmeta::Cli::parse();

    match exifmeta::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}
