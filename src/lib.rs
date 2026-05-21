use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use little_exif::exif_tag::ExifTag;
use little_exif::metadata::Metadata;

pub mod cli;
pub mod version;

pub use cli::{Cli, Command, RunArgs};

pub fn run(cli: Cli) -> Result<(), String> {
    version::print_title();

    match cli.command {
        Command::Run(args) => run_command(cli.dry_run, args),
        Command::Init => stub_command(cli.dry_run, "init"),
        Command::Validate => stub_command(cli.dry_run, "validate"),
        Command::Inspect { image } => inspect_command(image),
        Command::Interactive => stub_command(cli.dry_run, "interactive"),
        Command::Strip => stub_command(cli.dry_run, "strip"),
    }
}

fn inspect_command(image: PathBuf) -> Result<(), String> {
    validate_image_path(&image)?;

    let metadata = read_metadata(&image)?;
    let tags = metadata.into_iter().cloned().collect::<Vec<_>>();

    println!("{}", format_inspect_output(&image, &tags));

    Ok(())
}

fn read_metadata(image: &Path) -> Result<Metadata, String> {
    catch_unwind(AssertUnwindSafe(|| Metadata::new_from_path(image)))
        .map_err(|payload| {
            format!(
                "failed to read EXIF metadata from {}: {}",
                image.display(),
                panic_message(payload)
            )
        })?
        .map_err(|error| {
            format!(
                "failed to read EXIF metadata from {}: {error}",
                image.display()
            )
        })
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }

    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }

    "unexpected parser failure".to_string()
}

fn validate_image_path(image: &Path) -> Result<(), String> {
    if !image.exists() {
        return Err(format!("image does not exist: {}", image.display()));
    }

    if !image.is_file() {
        return Err(format!("image path is not a file: {}", image.display()));
    }

    Ok(())
}

fn format_inspect_output(image: &Path, tags: &[ExifTag]) -> String {
    let mut output = format!("EXIF metadata for {}\n", image.display());

    if tags.is_empty() {
        output.push_str("No EXIF metadata found.");
        return output;
    }

    let mut rows = tags
        .iter()
        .map(|tag| InspectRow {
            group: format!("{:?}", tag.get_group()),
            tag_id: tag.as_u16(),
            format: format!("{:?}", tag.format()),
            value: format!("{tag:?}"),
        })
        .collect::<Vec<_>>();

    rows.sort_by(|left, right| {
        left.group
            .cmp(&right.group)
            .then(left.tag_id.cmp(&right.tag_id))
            .then(left.value.cmp(&right.value))
    });

    let group_width = rows.iter().map(|row| row.group.len()).max().unwrap_or(0);
    let format_width = rows.iter().map(|row| row.format.len()).max().unwrap_or(0);

    for row in rows {
        output.push_str(&format!(
            "{:<group_width$}  0x{:04X}  {:<format_width$}  {}\n",
            row.group, row.tag_id, row.format, row.value
        ));
    }

    output.trim_end().to_string()
}

struct InspectRow {
    group: String,
    tag_id: u16,
    format: String,
    value: String,
}

fn run_command(dry_run: bool, args: RunArgs) -> Result<(), String> {
    let mut details = Vec::new();

    if dry_run {
        details.push("dry-run".to_string());
    }

    if args.strip {
        details.push("strip".to_string());
    }

    if args.no_overwrite {
        details.push("no-overwrite".to_string());
    }

    if args.recursive {
        details.push("recursive".to_string());
    }

    if !args.extensions.is_empty() {
        details.push(format!("extensions={}", args.extensions.join(",")));
    }

    if details.is_empty() {
        println!("run: not implemented yet");
    } else {
        println!("run: not implemented yet ({})", details.join(", "));
    }

    Ok(())
}

fn stub_command(dry_run: bool, command: &str) -> Result<(), String> {
    if dry_run {
        println!("{command}: not implemented yet (dry-run)");
    } else {
        println!("{command}: not implemented yet");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use little_exif::exif_tag::ExifTag;

    #[test]
    fn formats_empty_inspect_output() {
        assert_eq!(
            format_inspect_output(Path::new("image.tif"), &[]),
            "EXIF metadata for image.tif\nNo EXIF metadata found."
        );
    }

    #[test]
    fn formats_inspect_output_sorted_by_group_tag_and_value() {
        let tags = vec![
            ExifTag::Model("ETRS".to_string()),
            ExifTag::DateTimeOriginal("2026:04:28 00:00:00".to_string()),
            ExifTag::Make("Zenza Bronica".to_string()),
        ];

        let output = format_inspect_output(Path::new("image.tif"), &tags);

        assert_eq!(
            output,
            "\
EXIF metadata for image.tif
EXIF     0x9003  STRING  DateTimeOriginal(\"2026:04:28 00:00:00\")
GENERIC  0x010F  STRING  Make(\"Zenza Bronica\")
GENERIC  0x0110  STRING  Model(\"ETRS\")"
        );
    }

    #[test]
    fn rejects_missing_image_path() {
        let missing = Path::new("definitely-missing-image.tif");

        assert_eq!(
            validate_image_path(missing),
            Err("image does not exist: definitely-missing-image.tif".to_string())
        );
    }

    #[test]
    fn rejects_directory_image_path() {
        assert_eq!(
            validate_image_path(Path::new(".")),
            Err("image path is not a file: .".to_string())
        );
    }
}
