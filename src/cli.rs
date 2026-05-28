use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Parser, PartialEq, Eq)]
#[command(name = "exifmeta")]
#[command(about = "Read metadata.yml and write EXIF metadata to image files")]
#[command(version, propagate_version = true)]
pub struct Cli {
    #[arg(long, global = true, help = "Simulate actions without changing files")]
    pub dry_run: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Clone, Subcommand, PartialEq, Eq)]
pub enum Command {
    #[command(about = "Create metadata.yml file")]
    New(NewArgs),

    #[command(about = "Checks metadata.yml file is valid")]
    Check(CheckArgs),

    #[command(about = "Read an image file's EXIF tags")]
    Read(ReadArgs),

    #[command(about = "Writes EXIF tags defined in metadata.yml to target image files")]
    Write(WriteArgs),

    #[command(about = "Tool to remove all (or a select few) EXIF tags from target image files")]
    Strip(StripArgs),

    #[command(about = "Interactively browse folders and read image EXIF tags")]
    Interactive(InteractiveArgs),
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct NewArgs {
    #[arg(value_name = "DIRECTORY", default_value = ".")]
    pub path: PathBuf,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct CheckArgs {
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct InteractiveArgs {
    #[arg(value_name = "DIRECTORY", default_value = ".")]
    pub path: PathBuf,
}

#[derive(Debug, Clone, Args, PartialEq, Eq)]
pub struct WriteArgs {
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
pub struct ReadArgs {
    #[arg(value_name = "IMAGE")]
    pub image: PathBuf,

    #[arg(
        long,
        value_enum,
        default_value_t = ReadFormat::Pretty,
        help = "Choose the read output format"
    )]
    pub format: ReadFormat,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum ReadFormat {
    Pretty,
    Raw,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn help_lists_commands_in_documented_order() {
        let mut command = Cli::command();
        let help = command.render_help().to_string();
        let expected_order = ["new", "check", "read", "write", "strip", "interactive"];
        let mut previous_position = 0;

        for expected_command in expected_order {
            let position = help
                .find(&format!("  {expected_command}"))
                .unwrap_or_else(|| panic!("expected help to list {expected_command} command"));

            assert!(
                position >= previous_position,
                "expected {expected_command} to appear after previous command in help:\n{help}"
            );

            previous_position = position;
        }

        let help_position = help
            .find("  help")
            .expect("expected help to list generated help command");
        assert!(
            help_position >= previous_position,
            "expected generated help command to appear after app commands:\n{help}"
        );
    }

    #[test]
    fn help_lists_readme_command_descriptions() {
        let mut command = Cli::command();
        let help = command.render_help().to_string();
        let expected_descriptions = [
            "Create metadata.yml file",
            "Checks metadata.yml file is valid",
            "Read an image file's EXIF tags",
            "Writes EXIF tags defined in metadata.yml to target image files",
            "Tool to remove all (or a select few) EXIF tags from target image files",
            "Interactively browse folders and read image EXIF tags",
        ];

        for expected_description in expected_descriptions {
            assert!(
                help.contains(expected_description),
                "expected help to include README command description {expected_description:?}:\n{help}"
            );
        }
    }

    #[test]
    fn parses_each_readme_command() {
        for command in ["new", "check", "write", "strip", "interactive"] {
            assert!(
                Cli::try_parse_from(["exifmeta", command]).is_ok(),
                "expected {command} to parse"
            );
        }

        assert!(Cli::try_parse_from(["exifmeta", "read", "image.jpg"]).is_ok());
    }

    #[test]
    fn parses_new_default_path() {
        let cli = Cli::try_parse_from(["exifmeta", "new"]).expect("new should parse");

        let Command::New(args) = cli.command else {
            panic!("expected new command");
        };

        assert_eq!(args.path, PathBuf::from("."));
    }

    #[test]
    fn parses_new_path() {
        let cli = Cli::try_parse_from(["exifmeta", "new", "some/other/directory"])
            .expect("new path should parse");

        let Command::New(args) = cli.command else {
            panic!("expected new command");
        };

        assert_eq!(args.path, PathBuf::from("some/other/directory"));
    }

    #[test]
    fn parses_interactive_default_path() {
        let cli =
            Cli::try_parse_from(["exifmeta", "interactive"]).expect("interactive should parse");

        let Command::Interactive(args) = cli.command else {
            panic!("expected interactive command");
        };

        assert_eq!(args.path, PathBuf::from("."));
    }

    #[test]
    fn parses_interactive_path() {
        let cli = Cli::try_parse_from(["exifmeta", "interactive", "some/photos"])
            .expect("interactive path should parse");

        let Command::Interactive(args) = cli.command else {
            panic!("expected interactive command");
        };

        assert_eq!(args.path, PathBuf::from("some/photos"));
    }

    #[test]
    fn parses_read_default_format() {
        let cli = Cli::try_parse_from(["exifmeta", "read", "image.jpg"])
            .expect("read command should parse");

        let Command::Read(args) = cli.command else {
            panic!("expected read command");
        };

        assert_eq!(args.image, PathBuf::from("image.jpg"));
        assert_eq!(args.format, ReadFormat::Pretty);
    }

    #[test]
    fn parses_read_raw_format() {
        let cli = Cli::try_parse_from(["exifmeta", "read", "image.jpg", "--format", "raw"])
            .expect("read raw format should parse");

        let Command::Read(args) = cli.command else {
            panic!("expected read command");
        };

        assert_eq!(args.format, ReadFormat::Raw);
    }

    #[test]
    fn rejects_invalid_read_format() {
        assert!(
            Cli::try_parse_from(["exifmeta", "read", "image.jpg", "--format", "json"]).is_err()
        );
    }

    #[test]
    fn rejects_write_flags_on_read() {
        assert!(Cli::try_parse_from(["exifmeta", "read", "image.jpg", "--recursive"]).is_err());
    }

    #[test]
    fn parses_write_flags() {
        let cli = Cli::try_parse_from([
            "exifmeta",
            "--dry-run",
            "write",
            "--strip",
            "--no-overwrite",
            "--extensions",
            "jpg,tiff",
            "--recursive",
        ])
        .expect("write flags should parse");

        assert!(cli.dry_run);

        let Command::Write(args) = cli.command else {
            panic!("expected write command");
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
    fn parses_write_keep() {
        let cli = Cli::try_parse_from(["exifmeta", "write", "--keep", "Make,Model"])
            .expect("write keep should parse");

        let Command::Write(args) = cli.command else {
            panic!("expected write command");
        };

        assert!(!args.strip);
        assert_eq!(args.keep, ["Make", "Model"]);
        assert!(args.remove.is_empty());
        assert!(!args.privacy);
    }

    #[test]
    fn parses_write_repeated_remove_values() {
        let cli = Cli::try_parse_from([
            "exifmeta",
            "write",
            "--remove",
            "GPSLatitude,GPSLongitude",
            "--remove",
            "UserComment",
        ])
        .expect("write repeated remove values should parse");

        let Command::Write(args) = cli.command else {
            panic!("expected write command");
        };

        assert_eq!(args.remove, ["GPSLatitude", "GPSLongitude", "UserComment"]);
    }

    #[test]
    fn parses_write_remove_with_privacy() {
        let cli = Cli::try_parse_from(["exifmeta", "write", "--privacy", "--remove", "FNumber"])
            .expect("write remove should compose with privacy");

        let Command::Write(args) = cli.command else {
            panic!("expected write command");
        };

        assert!(args.privacy);
        assert_eq!(args.remove, ["FNumber"]);
    }

    #[test]
    fn rejects_conflicting_write_strip_modes() {
        assert!(Cli::try_parse_from(["exifmeta", "write", "--privacy", "--keep", "Make"]).is_err());
        assert!(Cli::try_parse_from(["exifmeta", "write", "--strip", "--keep", "Make"]).is_err());
        assert!(Cli::try_parse_from(["exifmeta", "write", "--strip", "--remove", "Make"]).is_err());
        assert!(Cli::try_parse_from(["exifmeta", "write", "--strip", "--privacy"]).is_err());
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
        let cli = Cli::try_parse_from(["exifmeta", "check", "--dry-run"])
            .expect("global dry-run should parse after subcommands");

        assert!(cli.dry_run);
        assert_eq!(cli.command, Command::Check(CheckArgs { path: None }));
    }

    #[test]
    fn parses_check_default_path() {
        let cli = Cli::try_parse_from(["exifmeta", "check"]).expect("check should parse");

        let Command::Check(args) = cli.command else {
            panic!("expected check command");
        };

        assert_eq!(args.path, None);
    }

    #[test]
    fn parses_check_path() {
        let cli = Cli::try_parse_from(["exifmeta", "check", "some/metadata.yml"])
            .expect("check path should parse");

        let Command::Check(args) = cli.command else {
            panic!("expected check command");
        };

        assert_eq!(args.path, Some(PathBuf::from("some/metadata.yml")));
    }

    #[test]
    fn rejects_unknown_commands() {
        assert!(Cli::try_parse_from(["exifmeta", "unknown"]).is_err());
    }

    #[test]
    fn rejects_renamed_commands() {
        for command in ["run", "init", "validate"] {
            assert!(
                Cli::try_parse_from(["exifmeta", command]).is_err(),
                "expected old {command} command to be rejected"
            );
        }

        assert!(Cli::try_parse_from(["exifmeta", "inspect", "image.jpg"]).is_err());
    }
}
