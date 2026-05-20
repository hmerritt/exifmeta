use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(name = "exifmeta")]
#[command(about = "Read metadata.yaml and write EXIF metadata to image files")]
#[command(version, propagate_version = true)]
pub struct Cli {
    #[arg(long, global = true, help = "Simulate actions without changing files")]
    pub dry_run: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum Command {
    #[command(about = "Read metadata.yaml and write EXIF data to target image files")]
    Run(RunArgs),

    #[command(about = "Create a template metadata.yaml file")]
    Init,

    #[command(about = "Check metadata.yaml is valid")]
    Validate,

    #[command(about = "Read and pretty-print the current EXIF data of an image file")]
    Inspect {
        #[arg(value_name = "IMAGE")]
        image: PathBuf,
    },

    #[command(about = "Interactively read and set EXIF data for an image")]
    Interactive,

    #[command(about = "Remove all existing EXIF metadata from target image files")]
    Strip,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct RunArgs {
    #[arg(long, help = "Remove existing EXIF data before adding new data")]
    pub strip: bool,

    #[arg(long, help = "Prevent overwriting existing EXIF data")]
    pub no_overwrite: bool,

    #[arg(
        short = 'e',
        long,
        value_delimiter = ',',
        value_name = "EXTENSIONS",
        help = "Restrict processing to comma-separated file extensions"
    )]
    pub extensions: Vec<String>,

    #[arg(long, help = "Find image files across all subdirectories")]
    pub recursive: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_each_readme_command() {
        for command in ["run", "init", "validate", "interactive", "strip"] {
            assert!(
                Cli::try_parse_from(["exifmeta", command]).is_ok(),
                "expected {command} to parse"
            );
        }

        assert!(Cli::try_parse_from(["exifmeta", "inspect", "image.jpg"]).is_ok());
    }

    #[test]
    fn parses_run_flags() {
        let cli = Cli::try_parse_from([
            "exifmeta",
            "--dry-run",
            "run",
            "--strip",
            "--no-overwrite",
            "--extensions",
            "jpg,tiff",
            "--recursive",
        ])
        .expect("run flags should parse");

        assert!(cli.dry_run);

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };

        assert!(args.strip);
        assert!(args.no_overwrite);
        assert!(args.recursive);
        assert_eq!(args.extensions, ["jpg", "tiff"]);
    }

    #[test]
    fn parses_global_dry_run_after_subcommand() {
        let cli = Cli::try_parse_from(["exifmeta", "validate", "--dry-run"])
            .expect("global dry-run should parse after subcommands");

        assert!(cli.dry_run);
        assert_eq!(cli.command, Command::Validate);
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(Cli::try_parse_from(["exifmeta", "unknown"]).is_err());
    }
}
