use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use exif::{Exif, Field, Reader, Tag, Value};

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

    println!("{}", format_inspect_output(&image, &metadata));

    Ok(())
}

fn read_metadata(image: &Path) -> Result<InspectMetadata, String> {
    let file = File::open(image)
        .map_err(|error| format!("failed to open {}: {error}", image.display()))?;
    let mut reader = BufReader::new(file);
    let mut warnings = Vec::new();

    let exif = Reader::new()
        .continue_on_error(true)
        .read_from_container(&mut reader)
        .or_else(|error| {
            error.distill_partial_result(|errors| {
                warnings.extend(errors.into_iter().map(|error| error.to_string()));
            })
        })
        .map_err(|error| {
            format!(
                "failed to read EXIF metadata from {}: {error}",
                image.display()
            )
        })?;

    Ok(InspectMetadata { exif, warnings })
}

struct InspectMetadata {
    exif: Exif,
    warnings: Vec<String>,
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

fn format_inspect_output(image: &Path, metadata: &InspectMetadata) -> String {
    let mut output = format!("EXIF metadata for {}\n", image.display());
    let mut rows = metadata
        .exif
        .fields()
        .map(|field| InspectRow::from_field(field, &metadata.exif))
        .collect::<Vec<_>>();

    if rows.is_empty() {
        output.push_str("No EXIF metadata found.");
    } else {
        rows.sort_by(|left, right| {
            left.is_unknown
                .cmp(&right.is_unknown)
                .then(left.ifd.cmp(&right.ifd))
                .then(left.context.cmp(&right.context))
                .then(left.tag_id.cmp(&right.tag_id))
                .then(left.name.cmp(&right.name))
        });

        let status_width = rows.iter().map(|row| row.status.len()).max().unwrap_or(0);
        let context_width = rows.iter().map(|row| row.context.len()).max().unwrap_or(0);
        let name_width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);

        for row in rows {
            output.push_str(&format!(
                "{:<status_width$}  IFD {}  {:<context_width$}  0x{:04X}  {:<name_width$}  {}\n",
                row.status, row.ifd, row.context, row.tag_id, row.name, row.value
            ));
        }

        output = output.trim_end().to_string();
    }

    if !metadata.warnings.is_empty() {
        output.push_str("\n\nWarnings:\n");
        for warning in &metadata.warnings {
            output.push_str(&format!("warning: {warning}\n"));
        }
        output = output.trim_end().to_string();
    }

    output
}

struct InspectRow {
    status: &'static str,
    is_unknown: bool,
    ifd: usize,
    context: String,
    tag_id: u16,
    name: String,
    value: String,
}

impl InspectRow {
    fn from_field(field: &Field, exif: &Exif) -> Self {
        let is_unknown =
            field.tag.description().is_none() || matches!(field.value, Value::Unknown(..));
        let name = if is_unknown {
            format!(
                "Tag({:?}, 0x{:04X})",
                field.tag.context(),
                field.tag.number()
            )
        } else {
            field.tag.to_string()
        };
        let mut value = if is_unknown {
            format!("{:?}", field.value)
        } else {
            field.display_value().with_unit(exif).to_string()
        };

        if !is_unknown {
            if let Some(decimal) = decimal_gps_coordinate(field, exif) {
                value.push_str(&format!(" ({})", format_decimal_coordinate(decimal)));
            }
        }

        Self {
            status: if is_unknown { "UNKNOWN" } else { "KNOWN" },
            is_unknown,
            ifd: usize::from(field.ifd_num.index()),
            context: format!("{:?}", field.tag.context()),
            tag_id: field.tag.number(),
            name,
            value,
        }
    }
}

fn decimal_gps_coordinate(field: &Field, exif: &Exif) -> Option<f64> {
    let reference_tag = match field.tag {
        Tag::GPSLatitude => Tag::GPSLatitudeRef,
        Tag::GPSLongitude => Tag::GPSLongitudeRef,
        _ => return None,
    };

    let values = match field.value {
        Value::Rational(ref values) => values,
        _ => return None,
    };
    let [degrees, minutes, seconds] = values.get(..3)? else {
        return None;
    };

    let decimal = degrees.to_f64() + minutes.to_f64() / 60.0 + seconds.to_f64() / 3600.0;
    if !decimal.is_finite() {
        return None;
    }

    let sign = exif
        .get_field(reference_tag, field.ifd_num)
        .and_then(gps_reference)
        .map_or(1.0, |reference| match reference {
            "S" | "W" => -1.0,
            _ => 1.0,
        });

    Some(decimal * sign)
}

fn gps_reference(field: &Field) -> Option<&str> {
    let Value::Ascii(ref values) = field.value else {
        return None;
    };
    let bytes = values.first()?;

    std::str::from_utf8(bytes).ok()
}

fn format_decimal_coordinate(value: f64) -> String {
    let formatted = format!("{value:.6}");

    formatted
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
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

    use exif::{Context, Tag};

    #[test]
    fn formats_empty_inspect_output() {
        assert_eq!(
            format_inspect_output(
                Path::new("image.tif"),
                &InspectMetadata {
                    exif: parse_raw_exif(&[]),
                    warnings: Vec::new(),
                },
            ),
            "EXIF metadata for image.tif\nNo EXIF metadata found."
        );
    }

    #[test]
    fn formats_inspect_output_with_unknown_rows_at_the_bottom() {
        let metadata = InspectMetadata {
            exif: parse_raw_exif(&[
                tiff_ascii_entry(0x010f, b"Z\0"),
                tiff_short_entry(0xfde8, 42),
                tiff_ascii_entry(0x0110, b"E\0"),
            ]),
            warnings: vec!["ignored malformed trailing field".to_string()],
        };

        let output = format_inspect_output(Path::new("image.tif"), &metadata);

        assert!(output.contains("KNOWN"));
        assert!(output.contains("UNKNOWN"));
        assert!(output.contains("0x010F"));
        assert!(output.contains("0x0110"));
        assert!(output.contains("0xFDE8"));
        assert!(output.contains("Warnings:"));
        assert!(output.contains("warning: ignored malformed trailing field"));
        assert!(output.find("0x0110").unwrap() < output.find("0xFDE8").unwrap());
    }

    #[test]
    fn reads_unknown_tags_without_failing() {
        let exif = parse_raw_exif(&[tiff_short_entry(0xfde8, 42)]);
        let fields = exif.fields().collect::<Vec<_>>();

        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].tag, Tag(Context::Tiff, 0xfde8));
        assert!(fields[0].tag.description().is_none());
    }

    #[test]
    fn appends_signed_decimal_gps_coordinates() {
        let (latitude_entry, latitude_data) =
            tiff_rational_entry(0x0002, [(52, 1), (21, 1), (101952, 10000)], 200);
        let (longitude_entry, longitude_data) =
            tiff_rational_entry(0x0004, [(1, 1), (18, 1), (1471968, 100000)], 224);
        let metadata = InspectMetadata {
            exif: parse_raw_exif_with_gps_entries(
                &[
                    tiff_ascii_entry(0x0001, b"N\0"),
                    latitude_entry,
                    tiff_ascii_entry(0x0003, b"W\0"),
                    longitude_entry,
                ],
                &[(200, latitude_data), (224, longitude_data)],
            ),
            warnings: Vec::new(),
        };

        let output = format_inspect_output(Path::new("image.jpg"), &metadata);

        assert!(output.contains("GPSLatitude"));
        assert!(output.contains("(52.352832)"));
        assert!(output.contains("GPSLongitude"));
        assert!(output.contains("(-1.304089)"));
    }

    #[test]
    fn appends_unsigned_decimal_gps_coordinate_when_ref_is_missing() {
        let (latitude_entry, latitude_data) =
            tiff_rational_entry(0x0002, [(10, 1), (30, 1), (0, 1)], 200);
        let metadata = InspectMetadata {
            exif: parse_raw_exif_with_gps_entries(&[latitude_entry], &[(200, latitude_data)]),
            warnings: Vec::new(),
        };

        let output = format_inspect_output(Path::new("image.jpg"), &metadata);

        assert!(output.contains("GPSLatitude"));
        assert!(output.contains("[GPSLatitudeRef missing]"));
        assert!(output.contains("(10.5)"));
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

    fn parse_raw_exif(entries: &[[u8; 12]]) -> Exif {
        parse_raw_exif_with_offsets(entries, &[])
    }

    fn parse_raw_exif_with_offsets(entries: &[[u8; 12]], offset_data: &[(u32, Vec<u8>)]) -> Exif {
        let mut data = vec![0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(&(entries.len() as u16).to_be_bytes());
        for entry in entries {
            data.extend_from_slice(entry);
        }
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut offset_data = offset_data.to_vec();
        offset_data.sort_by_key(|(offset, _)| *offset);
        for (offset, bytes) in offset_data {
            let offset = offset as usize;
            if data.len() < offset {
                data.resize(offset, 0);
            }
            data.extend_from_slice(&bytes);
        }

        Reader::new()
            .continue_on_error(true)
            .read_raw(data)
            .or_else(|error| error.distill_partial_result(|_| {}))
            .expect("test EXIF should parse")
    }

    fn parse_raw_exif_with_gps_entries(
        gps_entries: &[[u8; 12]],
        offset_data: &[(u32, Vec<u8>)],
    ) -> Exif {
        let mut data = vec![0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(&1u16.to_be_bytes());
        data.extend_from_slice(&tiff_long_entry(0x8825, 100));
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        data.resize(100, 0);
        data.extend_from_slice(&(gps_entries.len() as u16).to_be_bytes());
        for entry in gps_entries {
            data.extend_from_slice(entry);
        }
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        let mut offset_data = offset_data.to_vec();
        offset_data.sort_by_key(|(offset, _)| *offset);
        for (offset, bytes) in offset_data {
            let offset = offset as usize;
            if data.len() < offset {
                data.resize(offset, 0);
            }
            data.extend_from_slice(&bytes);
        }

        Reader::new()
            .continue_on_error(true)
            .read_raw(data)
            .or_else(|error| error.distill_partial_result(|_| {}))
            .expect("test EXIF should parse")
    }

    fn tiff_ascii_entry(tag: u16, value: &[u8]) -> [u8; 12] {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&2u16.to_be_bytes());
        entry[4..8].copy_from_slice(&(value.len() as u32).to_be_bytes());
        entry[8..(8 + value.len())].copy_from_slice(value);
        entry
    }

    fn tiff_short_entry(tag: u16, value: u16) -> [u8; 12] {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&3u16.to_be_bytes());
        entry[4..8].copy_from_slice(&1u32.to_be_bytes());
        entry[8..10].copy_from_slice(&value.to_be_bytes());
        entry
    }

    fn tiff_long_entry(tag: u16, value: u32) -> [u8; 12] {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&4u16.to_be_bytes());
        entry[4..8].copy_from_slice(&1u32.to_be_bytes());
        entry[8..12].copy_from_slice(&value.to_be_bytes());
        entry
    }

    fn tiff_rational_entry(tag: u16, values: [(u32, u32); 3], offset: u32) -> ([u8; 12], Vec<u8>) {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&5u16.to_be_bytes());
        entry[4..8].copy_from_slice(&3u32.to_be_bytes());
        entry[8..12].copy_from_slice(&offset.to_be_bytes());

        let mut data = Vec::new();
        for (numerator, denominator) in values {
            data.extend_from_slice(&numerator.to_be_bytes());
            data.extend_from_slice(&denominator.to_be_bytes());
        }

        (entry, data)
    }
}
