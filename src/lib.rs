use std::fs::{self, File, Metadata};
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Local};
use exif::{Exif, Field, Reader, Tag, Value};

pub mod cli;
pub mod version;

pub use cli::{Cli, Command, InspectArgs, InspectFormat, RunArgs};

pub fn run(cli: Cli) -> Result<(), String> {
    version::print_title();

    match cli.command {
        Command::Run(args) => run_command(cli.dry_run, args),
        Command::Init => stub_command(cli.dry_run, "init"),
        Command::Validate => stub_command(cli.dry_run, "validate"),
        Command::Inspect(args) => inspect_command(args),
        Command::Interactive => stub_command(cli.dry_run, "interactive"),
        Command::Strip => stub_command(cli.dry_run, "strip"),
    }
}

fn inspect_command(args: InspectArgs) -> Result<(), String> {
    let image = args.image;
    validate_image_path(&image)?;

    let metadata = read_metadata(&image)?;

    println!("{}", format_inspect_output(&image, &metadata, args.format));

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

    let file_info = InspectFileInfo::from_path(image, &exif)?;

    Ok(InspectMetadata {
        exif,
        warnings,
        file_info,
    })
}

struct InspectMetadata {
    exif: Exif,
    warnings: Vec<String>,
    file_info: InspectFileInfo,
}

struct InspectFileInfo {
    rows: Vec<InspectInfoRow>,
}

impl InspectFileInfo {
    fn from_path(image: &Path, exif: &Exif) -> Result<Self, String> {
        let metadata = fs::metadata(image).map_err(|error| {
            format!(
                "failed to read file metadata for {}: {error}",
                image.display()
            )
        })?;
        let file_kind = detect_file_kind(image);
        let mut rows = Vec::new();

        rows.push(InspectInfoRow::new(
            "Exifmeta Version Number",
            version::get_version().version_number(),
        ));
        rows.push(InspectInfoRow::new("File Name", file_name(image)));
        rows.push(InspectInfoRow::new("Directory", directory_name(image)));
        rows.push(InspectInfoRow::new(
            "File Size",
            format_file_size(metadata.len()),
        ));

        if let Ok(modified) = metadata.modified() {
            rows.push(InspectInfoRow::new(
                "File Modification Date/Time",
                format_system_time(modified),
            ));
        }

        if let Ok(accessed) = metadata.accessed() {
            rows.push(InspectInfoRow::new(
                "File Access Date/Time",
                format_system_time(accessed),
            ));
        }

        if let Ok(created) = metadata.created() {
            rows.push(InspectInfoRow::new(
                "File Creation Date/Time",
                format_system_time(created),
            ));
        }

        rows.push(InspectInfoRow::new(
            "File Permissions",
            format_permissions(&metadata),
        ));
        rows.push(InspectInfoRow::new("File Type", file_kind.file_type));
        rows.push(InspectInfoRow::new(
            "File Type Extension",
            file_kind.extension,
        ));
        rows.push(InspectInfoRow::new("MIME Type", file_kind.mime_type));
        rows.push(InspectInfoRow::new(
            "Exif Byte Order",
            format_exif_byte_order(exif),
        ));

        Ok(Self { rows })
    }

    #[cfg(test)]
    fn empty() -> Self {
        Self { rows: Vec::new() }
    }
}

struct InspectInfoRow {
    name: String,
    value: String,
}

impl InspectInfoRow {
    fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }
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

fn format_inspect_output(
    _image: &Path,
    metadata: &InspectMetadata,
    format: InspectFormat,
) -> String {
    let mut output = String::new();
    let mut rows = metadata
        .exif
        .fields()
        .map(|field| InspectRow::from_field(field, &metadata.exif))
        .collect::<Vec<_>>();

    if rows.is_empty() {
        append_info_rows(&mut output, &metadata.file_info.rows, None);
        output.push_str("No EXIF metadata found.");
    } else {
        sort_inspect_rows(&mut rows);

        match format {
            InspectFormat::Pretty => {
                append_pretty_inspect_rows(&mut output, &metadata.file_info.rows, &rows)
            }
            InspectFormat::Raw => {
                append_info_rows(&mut output, &metadata.file_info.rows, None);
                append_raw_inspect_rows(&mut output, &rows);
            }
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

fn append_info_rows(output: &mut String, rows: &[InspectInfoRow], width: Option<usize>) {
    if rows.is_empty() {
        return;
    }

    let name_width =
        width.unwrap_or_else(|| rows.iter().map(|row| row.name.len()).max().unwrap_or(0));

    for row in rows {
        output.push_str(&format!("{:<name_width$}  {}\n", row.name, row.value));
    }
}

fn sort_inspect_rows(rows: &mut [InspectRow]) {
    rows.sort_by(|left, right| {
        left.is_unknown
            .cmp(&right.is_unknown)
            .then(left.ifd.cmp(&right.ifd))
            .then(left.context.cmp(&right.context))
            .then(left.tag_id.cmp(&right.tag_id))
            .then(left.name.cmp(&right.name))
    });
}

fn append_raw_inspect_rows(output: &mut String, rows: &[InspectRow]) {
    let status_width = rows.iter().map(|row| row.status.len()).max().unwrap_or(0);
    let context_width = rows.iter().map(|row| row.context.len()).max().unwrap_or(0);
    let name_width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);

    for row in rows {
        output.push_str(&format!(
            "{:<status_width$}  IFD {}  {:<context_width$}  0x{:04X}  {:<name_width$}  {}\n",
            row.status, row.ifd, row.context, row.tag_id, row.name, row.value
        ));
    }
}

fn append_pretty_inspect_rows(
    output: &mut String,
    info_rows: &[InspectInfoRow],
    rows: &[InspectRow],
) {
    let name_width = info_rows
        .iter()
        .map(|row| row.name.len())
        .chain(rows.iter().map(|row| row.pretty_name.len()))
        .max()
        .unwrap_or(0);

    append_info_rows(output, info_rows, Some(name_width));

    for row in rows {
        output.push_str(&format!(
            "{:<name_width$}  {}\n",
            row.pretty_name, row.value
        ));
    }
}

struct FileKind {
    file_type: &'static str,
    extension: String,
    mime_type: &'static str,
}

fn detect_file_kind(image: &Path) -> FileKind {
    let signature = read_file_signature(image);
    let detected = signature.as_deref().and_then(file_kind_from_signature);
    let fallback = file_kind_from_extension(image);

    let (file_type, default_extension, mime_type) =
        detected
            .or(fallback)
            .unwrap_or(("Unknown", "", "application/octet-stream"));

    FileKind {
        file_type,
        extension: if default_extension.is_empty() {
            file_extension(image).unwrap_or_default()
        } else {
            default_extension.to_string()
        },
        mime_type,
    }
}

fn read_file_signature(image: &Path) -> Option<Vec<u8>> {
    let mut file = File::open(image).ok()?;
    let mut buffer = vec![0; 32];
    let length = file.read(&mut buffer).ok()?;
    buffer.truncate(length);
    Some(buffer)
}

fn file_kind_from_signature(
    signature: &[u8],
) -> Option<(&'static str, &'static str, &'static str)> {
    if signature.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some(("JPEG", "jpg", "image/jpeg"));
    }

    if signature.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(("PNG", "png", "image/png"));
    }

    if signature.starts_with(b"II*\0") || signature.starts_with(b"MM\0*") {
        return Some(("TIFF", "tif", "image/tiff"));
    }

    if signature.len() >= 12 && signature.starts_with(b"RIFF") && &signature[8..12] == b"WEBP" {
        return Some(("WEBP", "webp", "image/webp"));
    }

    if signature.starts_with(&[0xff, 0x0a]) || signature.starts_with(b"\0\0\0\x0cJXL ") {
        return Some(("JXL", "jxl", "image/jxl"));
    }

    if signature.len() >= 12 && &signature[4..8] == b"ftyp" {
        let brand = &signature[8..12];
        return match brand {
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"hevm" | b"hevs" => {
                Some(("HEIC", "heic", "image/heic"))
            }
            b"mif1" | b"msf1" => Some(("HEIF", "heif", "image/heif")),
            b"avif" | b"avis" => Some(("AVIF", "avif", "image/avif")),
            _ => None,
        };
    }

    None
}

fn file_kind_from_extension(image: &Path) -> Option<(&'static str, &'static str, &'static str)> {
    match file_extension(image)?.as_str() {
        "jpg" | "jpeg" => Some(("JPEG", "jpg", "image/jpeg")),
        "png" => Some(("PNG", "png", "image/png")),
        "tif" | "tiff" => Some(("TIFF", "tif", "image/tiff")),
        "webp" => Some(("WEBP", "webp", "image/webp")),
        "jxl" => Some(("JXL", "jxl", "image/jxl")),
        "heif" | "hif" => Some(("HEIF", "heif", "image/heif")),
        "heic" => Some(("HEIC", "heic", "image/heic")),
        "avif" => Some(("AVIF", "avif", "image/avif")),
        _ => None,
    }
}

fn file_extension(image: &Path) -> Option<String> {
    image
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

fn file_name(image: &Path) -> String {
    image
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| image.display().to_string(), ToString::to_string)
}

fn directory_name(image: &Path) -> String {
    let directory = image.parent().map(|parent| parent.display().to_string());

    match directory.as_deref() {
        Some("") | None => ".".to_string(),
        Some(directory) => directory.to_string(),
    }
}

fn format_file_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];

    if bytes < 1000 {
        return format!("{bytes} bytes");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }

    if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        let formatted = format!("{value:.1}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string();
        format!("{formatted} {}", UNITS[unit])
    }
}

fn format_system_time(time: SystemTime) -> String {
    let datetime: DateTime<Local> = time.into();

    datetime.format("%Y:%m:%d %H:%M:%S%:z").to_string()
}

#[cfg(unix)]
fn format_permissions(metadata: &Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode();
    let mut output = String::with_capacity(10);
    output.push(if metadata.is_dir() { 'd' } else { '-' });

    for shift in [6, 3, 0] {
        output.push(if mode & (0o4 << shift) != 0 { 'r' } else { '-' });
        output.push(if mode & (0o2 << shift) != 0 { 'w' } else { '-' });
        output.push(if mode & (0o1 << shift) != 0 { 'x' } else { '-' });
    }

    output
}

#[cfg(not(unix))]
fn format_permissions(metadata: &Metadata) -> String {
    if metadata.permissions().readonly() {
        "read-only".to_string()
    } else {
        "read-write".to_string()
    }
}

fn format_exif_byte_order(exif: &Exif) -> &'static str {
    if exif.little_endian() {
        "Little-endian (Intel, II)"
    } else {
        "Big-endian (Motorola, MM)"
    }
}

struct InspectRow {
    status: &'static str,
    is_unknown: bool,
    ifd: usize,
    context: String,
    tag_id: u16,
    name: String,
    pretty_name: String,
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
        let pretty_name = if is_unknown {
            format!("Unknown {} Tag", format!("{:?}", field.tag.context()))
        } else {
            title_case_tag_name(&name)
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
            pretty_name,
            value,
        }
    }
}

fn title_case_tag_name(name: &str) -> String {
    let spaced = decamelcase_tag_name(name);

    spaced
        .split_whitespace()
        .map(title_case_word)
        .collect::<Vec<_>>()
        .join(" ")
}

fn decamelcase_tag_name(name: &str) -> String {
    let mut output = String::new();
    let mut chars = name.chars().peekable();
    let mut previous: Option<char> = None;

    while let Some(current) = chars.next() {
        let next = chars.peek().copied();
        let needs_space = previous.is_some_and(|previous| {
            (previous.is_ascii_lowercase() && current.is_ascii_uppercase())
                || (previous.is_ascii_alphabetic() && current.is_ascii_digit())
                || (previous.is_ascii_uppercase()
                    && current.is_ascii_uppercase()
                    && next.is_some_and(|next| next.is_ascii_lowercase()))
        });

        if needs_space {
            output.push(' ');
        }

        output.push(current);
        previous = Some(current);
    }

    output
}

fn title_case_word(word: &str) -> String {
    if word
        .chars()
        .all(|char| char.is_ascii_uppercase() || char.is_ascii_digit())
    {
        return word.to_string();
    }

    let mut chars = word.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };

    let mut output = String::new();
    output.push(first.to_ascii_uppercase());
    output.extend(chars.map(|char| char.to_ascii_lowercase()));
    output
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
                    file_info: InspectFileInfo::empty(),
                },
                InspectFormat::Pretty,
            ),
            "No EXIF metadata found."
        );
    }

    #[test]
    fn formats_raw_inspect_output_with_unknown_rows_at_the_bottom() {
        let metadata = InspectMetadata {
            exif: parse_raw_exif(&[
                tiff_ascii_entry(0x010f, b"Z\0"),
                tiff_short_entry(0xfde8, 42),
                tiff_ascii_entry(0x0110, b"E\0"),
            ]),
            warnings: vec!["ignored malformed trailing field".to_string()],
            file_info: test_file_info(),
        };

        let output = format_inspect_output(Path::new("image.tif"), &metadata, InspectFormat::Raw);

        assert!(output.contains("KNOWN"));
        assert!(output.contains("UNKNOWN"));
        assert!(output.contains("File Name  image.tif"));
        assert!(output.contains("0x010F"));
        assert!(output.contains("0x0110"));
        assert!(output.contains("0xFDE8"));
        assert!(output.contains("Warnings:"));
        assert!(output.contains("warning: ignored malformed trailing field"));
        assert!(output.find("File Name").unwrap() < output.find("KNOWN").unwrap());
        assert!(output.find("0x0110").unwrap() < output.find("0xFDE8").unwrap());
    }

    #[test]
    fn formats_pretty_inspect_output_without_raw_columns() {
        let metadata = InspectMetadata {
            exif: parse_raw_exif(&[
                tiff_ascii_entry(0x010f, b"Z\0"),
                tiff_short_entry(0xfde8, 42),
                tiff_ascii_entry(0x0110, b"E\0"),
            ]),
            warnings: vec!["ignored malformed trailing field".to_string()],
            file_info: test_file_info(),
        };

        let output =
            format_inspect_output(Path::new("image.tif"), &metadata, InspectFormat::Pretty);

        assert!(output.contains("File Name         image.tif"));
        assert!(output.contains("Make              \"Z\""));
        assert!(output.contains("Model             \"E\""));
        assert!(output.contains("Unknown Tiff Tag  Short([42])"));
        assert!(output.contains("Warnings:"));
        assert!(output.contains("warning: ignored malformed trailing field"));
        assert!(output.find("File Name").unwrap() < output.find("Make").unwrap());
        assert!(!output.contains("KNOWN"));
        assert!(!output.contains("UNKNOWN"));
        assert!(!output.contains("IFD"));
        assert!(!output.contains("Tiff  "));
        assert!(!output.contains("0x010F"));
        assert!(!output.contains("0x0110"));
        assert!(!output.contains("0xFDE8"));
    }

    #[test]
    fn de_camelcases_tag_names_for_pretty_output() {
        assert_eq!(title_case_tag_name("GPSLatitude"), "GPS Latitude");
        assert_eq!(
            title_case_tag_name("DateTimeOriginal"),
            "Date Time Original"
        );
        assert_eq!(
            title_case_tag_name("FocalLengthIn35mmFilm"),
            "Focal Length In 35mm Film"
        );
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
    fn file_info_reports_file_and_exif_metadata() {
        let path = temporary_test_path("file-info.jpg");
        std::fs::write(&path, [0xff, 0xd8, 0xff]).expect("test image should be written");
        let exif = parse_raw_exif(&[]);
        let file_info =
            InspectFileInfo::from_path(&path, &exif).expect("file info should be collected");
        let rows = file_info.rows;

        assert!(info_row_value(&rows, "Exifmeta Version Number").is_some());
        let expected_file_name = path.file_name().and_then(|name| name.to_str());
        assert_eq!(info_row_value(&rows, "File Name"), expected_file_name);
        assert!(info_row_value(&rows, "Directory").is_some());
        assert_eq!(info_row_value(&rows, "File Size"), Some("3 bytes"));
        assert!(info_row_value(&rows, "File Permissions").is_some());
        assert_eq!(info_row_value(&rows, "File Type"), Some("JPEG"));
        assert_eq!(info_row_value(&rows, "File Type Extension"), Some("jpg"));
        assert_eq!(info_row_value(&rows, "MIME Type"), Some("image/jpeg"));
        assert_eq!(
            info_row_value(&rows, "Exif Byte Order"),
            Some("Big-endian (Motorola, MM)")
        );

        let _ = std::fs::remove_file(path);
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
            file_info: InspectFileInfo::empty(),
        };

        let output =
            format_inspect_output(Path::new("image.jpg"), &metadata, InspectFormat::Pretty);

        assert!(output.contains("GPS Latitude"));
        assert!(output.contains("(52.352832)"));
        assert!(output.contains("GPS Longitude"));
        assert!(output.contains("(-1.304089)"));
    }

    #[test]
    fn appends_unsigned_decimal_gps_coordinate_when_ref_is_missing() {
        let (latitude_entry, latitude_data) =
            tiff_rational_entry(0x0002, [(10, 1), (30, 1), (0, 1)], 200);
        let metadata = InspectMetadata {
            exif: parse_raw_exif_with_gps_entries(&[latitude_entry], &[(200, latitude_data)]),
            warnings: Vec::new(),
            file_info: InspectFileInfo::empty(),
        };

        let output =
            format_inspect_output(Path::new("image.jpg"), &metadata, InspectFormat::Pretty);

        assert!(output.contains("GPS Latitude"));
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

    fn test_file_info() -> InspectFileInfo {
        InspectFileInfo {
            rows: vec![InspectInfoRow::new("File Name", "image.tif")],
        }
    }

    fn info_row_value<'a>(rows: &'a [InspectInfoRow], name: &str) -> Option<&'a str> {
        rows.iter()
            .find(|row| row.name == name)
            .map(|row| row.value.as_str())
    }

    fn temporary_test_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("exifmeta-{}-{name}", std::process::id()))
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
