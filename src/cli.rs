use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

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
    Init(InitArgs),

    #[command(about = "Check metadata.yaml is valid")]
    Validate(ValidateArgs),

    #[command(about = "Read and pretty-print the current EXIF data of an image file")]
    Inspect(InspectArgs),

    #[command(about = "Interactively read and set EXIF data for an image")]
    Interactive,

    #[command(about = "Remove all existing EXIF metadata from target image files")]
    Strip(StripArgs),
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct InitArgs {
    #[arg(value_name = "DIRECTORY", default_value = ".")]
    pub path: PathBuf,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct ValidateArgs {
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct RunArgs {
    #[arg(value_name = "METADATA_OR_TARGETS")]
    pub metadata_or_targets: Option<PathBuf>,

    #[arg(value_name = "TARGETS")]
    pub targets: Option<String>,

    #[arg(
        long,
        conflicts_with_all = ["keep", "remove", "privacy"],
        help = "Remove existing EXIF data before adding new data"
    )]
    pub strip: bool,

    #[arg(
        long,
        value_delimiter = ',',
        value_name = "TAGS",
        conflicts_with = "privacy",
        help = "Strip all EXIF tags except the comma-separated tag names before adding new data"
    )]
    pub keep: Vec<String>,

    #[arg(
        long,
        value_delimiter = ',',
        value_name = "TAGS",
        help = "Remove only the comma-separated EXIF tag names before adding new data"
    )]
    pub remove: Vec<String>,

    #[arg(
        long,
        conflicts_with = "keep",
        help = "Remove privacy-sensitive EXIF tags before adding new data"
    )]
    pub privacy: bool,

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

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct StripArgs {
    #[arg(value_name = "TARGETS")]
    pub targets: Option<String>,

    #[arg(
        long,
        value_delimiter = ',',
        value_name = "TAGS",
        conflicts_with = "privacy",
        help = "Strip all EXIF tags except the comma-separated tag names"
    )]
    pub keep: Vec<String>,

    #[arg(
        long,
        value_delimiter = ',',
        value_name = "TAGS",
        help = "Remove only the comma-separated EXIF tag names"
    )]
    pub remove: Vec<String>,

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

    #[arg(long, help = "Verify that no EXIF metadata remains after stripping")]
    pub verify: bool,

    #[arg(
        long,
        conflicts_with = "keep",
        help = "Remove privacy-sensitive EXIF tags while keeping harmless technical tags"
    )]
    pub privacy: bool,

    #[arg(long, help = "Emit a machine-readable JSON report")]
    pub json: bool,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct InspectArgs {
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = InspectFormat::Pretty,
        help = "Choose the inspect output format"
    )]
    pub format: InspectFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum InspectFormat {
    Pretty,
    Raw,
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
    fn parses_init_default_path() {
        let cli = Cli::try_parse_from(["exifmeta", "init"]).expect("init should parse");

        let Command::Init(args) = cli.command else {
            panic!("expected init command");
        };

        assert_eq!(args.path, PathBuf::from("."));
    }

    #[test]
    fn parses_init_path() {
        let cli = Cli::try_parse_from(["exifmeta", "init", "some/other/directory"])
            .expect("init path should parse");

        let Command::Init(args) = cli.command else {
            panic!("expected init command");
        };

        assert_eq!(args.path, PathBuf::from("some/other/directory"));
    }

    #[test]
    fn parses_inspect_default_format() {
        let cli = Cli::try_parse_from(["exifmeta", "inspect", "image.jpg"])
            .expect("inspect command should parse");

        let Command::Inspect(args) = cli.command else {
            panic!("expected inspect command");
        };

        assert_eq!(args.image, PathBuf::from("image.jpg"));
        assert_eq!(args.format, InspectFormat::Pretty);
    }

    #[test]
    fn parses_inspect_raw_format() {
        let cli = Cli::try_parse_from(["exifmeta", "inspect", "image.jpg", "--format", "raw"])
            .expect("inspect raw format should parse");

        let Command::Inspect(args) = cli.command else {
            panic!("expected inspect command");
        };

        assert_eq!(args.format, InspectFormat::Raw);
    }

    #[test]
    fn rejects_invalid_inspect_format() {
        assert!(
            Cli::try_parse_from(["exifmeta", "inspect", "image.jpg", "--format", "json"]).is_err()
        );
    }

    #[test]
    fn rejects_run_flags_on_inspect() {
        assert!(Cli::try_parse_from(["exifmeta", "inspect", "image.jpg", "--recursive"]).is_err());
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
        assert!(args.keep.is_empty());
        assert!(args.remove.is_empty());
        assert!(!args.privacy);
        assert!(args.no_overwrite);
        assert!(args.recursive);
        assert_eq!(args.extensions, ["jpg", "tiff"]);
    }

    #[test]
    fn parses_run_keep() {
        let cli = Cli::try_parse_from(["exifmeta", "run", "--keep", "Make,Model"])
            .expect("run keep should parse");

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };

        assert!(!args.strip);
        assert_eq!(args.keep, ["Make", "Model"]);
        assert!(args.remove.is_empty());
        assert!(!args.privacy);
    }

    #[test]
    fn parses_run_repeated_remove_values() {
        let cli = Cli::try_parse_from([
            "exifmeta",
            "run",
            "--remove",
            "GPSLatitude,GPSLongitude",
            "--remove",
            "UserComment",
        ])
        .expect("run repeated remove values should parse");

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };

        assert_eq!(args.remove, ["GPSLatitude", "GPSLongitude", "UserComment"]);
    }

    #[test]
    fn parses_run_remove_with_privacy() {
        let cli = Cli::try_parse_from(["exifmeta", "run", "--privacy", "--remove", "FNumber"])
            .expect("run remove should compose with privacy");

        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };

        assert!(args.privacy);
        assert_eq!(args.remove, ["FNumber"]);
    }

    #[test]
    fn rejects_conflicting_run_strip_modes() {
        assert!(Cli::try_parse_from(["exifmeta", "run", "--privacy", "--keep", "Make"]).is_err());
        assert!(Cli::try_parse_from(["exifmeta", "run", "--strip", "--keep", "Make"]).is_err());
        assert!(Cli::try_parse_from(["exifmeta", "run", "--strip", "--remove", "Make"]).is_err());
        assert!(Cli::try_parse_from(["exifmeta", "run", "--strip", "--privacy"]).is_err());
    }

    #[test]
    fn parses_strip_default_args() {
        let cli = Cli::try_parse_from(["exifmeta", "strip"]).expect("strip should parse");

        let Command::Strip(args) = cli.command else {
            panic!("expected strip command");
        };

        assert_eq!(args.targets, None);
        assert!(args.keep.is_empty());
        assert!(args.remove.is_empty());
        assert!(args.extensions.is_empty());
        assert!(!args.recursive);
        assert!(!args.verify);
        assert!(!args.privacy);
        assert!(!args.json);
    }

    #[test]
    fn parses_strip_flags() {
        let cli = Cli::try_parse_from([
            "exifmeta",
            "--dry-run",
            "strip",
            "photos/*.jpg",
            "--keep",
            "Make,Model",
            "--recursive",
            "--extensions",
            "jpg,png",
            "--verify",
            "--json",
        ])
        .expect("strip flags should parse");

        assert!(cli.dry_run);

        let Command::Strip(args) = cli.command else {
            panic!("expected strip command");
        };

        assert_eq!(args.targets, Some("photos/*.jpg".to_string()));
        assert_eq!(args.keep, ["Make", "Model"]);
        assert!(args.remove.is_empty());
        assert_eq!(args.extensions, ["jpg", "png"]);
        assert!(args.recursive);
        assert!(args.verify);
        assert!(!args.privacy);
        assert!(args.json);
    }

    #[test]
    fn parses_strip_repeated_remove_values() {
        let cli = Cli::try_parse_from([
            "exifmeta",
            "strip",
            "--remove",
            "GPSLatitude,GPSLongitude",
            "--remove",
            "UserComment",
        ])
        .expect("strip repeated remove values should parse");

        let Command::Strip(args) = cli.command else {
            panic!("expected strip command");
        };

        assert_eq!(args.remove, ["GPSLatitude", "GPSLongitude", "UserComment"]);
    }

    #[test]
    fn parses_strip_remove_with_keep() {
        let cli = Cli::try_parse_from(["exifmeta", "strip", "--keep", "Make", "--remove", "Model"])
            .expect("strip remove should compose with keep");

        let Command::Strip(args) = cli.command else {
            panic!("expected strip command");
        };

        assert_eq!(args.keep, ["Make"]);
        assert_eq!(args.remove, ["Model"]);
    }

    #[test]
    fn parses_strip_remove_with_privacy() {
        let cli = Cli::try_parse_from(["exifmeta", "strip", "--privacy", "--remove", "FNumber"])
            .expect("strip remove should compose with privacy");

        let Command::Strip(args) = cli.command else {
            panic!("expected strip command");
        };

        assert!(args.privacy);
        assert_eq!(args.remove, ["FNumber"]);
    }

    #[test]
    fn rejects_conflicting_strip_modes() {
        assert!(Cli::try_parse_from(["exifmeta", "strip", "--privacy", "--keep", "Make"]).is_err());
    }

    #[test]
    fn parses_global_dry_run_after_subcommand() {
        let cli = Cli::try_parse_from(["exifmeta", "validate", "--dry-run"])
            .expect("global dry-run should parse after subcommands");

        assert!(cli.dry_run);
        assert_eq!(cli.command, Command::Validate(ValidateArgs { path: None }));
    }

    #[test]
    fn parses_validate_default_path() {
        let cli = Cli::try_parse_from(["exifmeta", "validate"]).expect("validate should parse");

        let Command::Validate(args) = cli.command else {
            panic!("expected validate command");
        };

        assert_eq!(args.path, None);
    }

    #[test]
    fn parses_validate_path() {
        let cli = Cli::try_parse_from(["exifmeta", "validate", "some/metadata.yaml"])
            .expect("validate path should parse");

        let Command::Validate(args) = cli.command else {
            panic!("expected validate command");
        };

        assert_eq!(args.path, Some(PathBuf::from("some/metadata.yaml")));
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(Cli::try_parse_from(["exifmeta", "unknown"]).is_err());
    }
}
