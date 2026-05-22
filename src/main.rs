use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    let cli = exifmeta::Cli::parse();

    match exifmeta::run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(exifmeta::CliError::Error(error)) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
        Err(exifmeta::CliError::Warning(warning)) => {
            eprintln!("warning: {warning}");
            ExitCode::FAILURE
        }
        Err(exifmeta::CliError::Failure) => ExitCode::FAILURE,
    }
}
