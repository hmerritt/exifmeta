use std::collections::HashSet;
use std::fs::{self, File, Metadata};
use std::io::{BufReader, Read};
use std::path::Path;
use std::ptr::NonNull;
use std::time::SystemTime;

use chrono::{DateTime, Local};
use colored::Colorize;
use exif::{Error as ExifError, Exif, Field, Reader, Tag, Value};
use rusqlite::{Connection, DatabaseName, params};

pub mod cli;
pub mod version;

pub use cli::{Cli, Command, InspectArgs, InspectFormat, RunArgs};

const GEONAMES_DATABASE: &[u8] = include_bytes!("../assets/geonames/cities1000.sqlite");
const NEAREST_LOCATION_LIMIT: usize = 5;
const EARTH_RADIUS_KM: f64 = 6_371.0088;

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
        .or_else(|error| match error {
            ExifError::NotFound(_) => Ok(empty_exif()),
            error => error.distill_partial_result(|errors| {
                warnings.extend(errors.into_iter().map(|error| error.to_string()));
            }),
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

fn empty_exif() -> Exif {
    Reader::new()
        .read_raw(vec![
            0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ])
        .expect("embedded empty EXIF should parse")
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
    let mut warnings = metadata.warnings.clone();
    let mut rows = metadata
        .exif
        .fields()
        .map(|field| InspectRow::from_field(field, &metadata.exif))
        .collect::<Vec<_>>();

    if rows.is_empty() {
        output.push_str(&format_empty_exif_message(format));
    } else {
        sort_inspect_rows(&mut rows);

        match format {
            InspectFormat::Pretty => append_pretty_inspect_rows(
                &mut output,
                &metadata.file_info.rows,
                &rows,
                &metadata.exif,
                &mut warnings,
            ),
            InspectFormat::Raw => {
                append_raw_inspect_rows(&mut output, &rows);
            }
        }

        output = output.trim_end().to_string();
    }

    if !warnings.is_empty() {
        output.push_str("\n\nWarnings:\n");
        for warning in &warnings {
            output.push_str(&format!("warning: {warning}\n"));
        }
        output = output.trim_end().to_string();
    }

    output
}

fn format_empty_exif_message(format: InspectFormat) -> String {
    match format {
        InspectFormat::Pretty => "<No EXIF metadata found>".yellow().to_string(),
        InspectFormat::Raw => "<No EXIF metadata found>".to_string(),
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
    let context_width = rows.iter().map(|row| row.context.len()).max().unwrap_or(0);
    let name_width = rows.iter().map(|row| row.name.len()).max().unwrap_or(0);

    for row in rows {
        output.push_str(&format!(
            "IFD {}  {:<context_width$}  0x{:04X}  {:<name_width$}  {}\n",
            row.ifd, row.context, row.tag_id, row.name, row.value
        ));
    }
}

fn append_pretty_inspect_rows(
    output: &mut String,
    info_rows: &[InspectInfoRow],
    rows: &[InspectRow],
    exif: &Exif,
    warnings: &mut Vec<String>,
) {
    let mut pretty_rows = pretty_inspect_rows(info_rows, rows);
    append_nearest_location_rows(&mut pretty_rows, exif, warnings);
    pretty_rows.sort_by(|left, right| {
        left.group
            .output_order()
            .cmp(&right.group.output_order())
            .then(left.label.cmp(&right.label))
            .then(left.value.cmp(&right.value))
    });

    let mut first_group = true;
    for group in PrettyInspectGroup::OUTPUT_ORDER {
        let group_rows = pretty_rows
            .iter()
            .filter(|row| row.group == group)
            .collect::<Vec<_>>();

        if group_rows.is_empty() {
            continue;
        }

        if !first_group {
            output.push('\n');
        }
        first_group = false;

        append_pretty_group_heading(output, group);

        let name_width = group_rows
            .iter()
            .map(|row| row.label.len())
            .max()
            .unwrap_or(0);
        for row in group_rows {
            output.push_str(&format!("{:<name_width$}  {}\n", row.label, row.value));
        }
    }
}

fn append_pretty_group_heading(output: &mut String, group: PrettyInspectGroup) {
    let title = group.title();
    output.push_str(&title.bright_blue().to_string());
    output.push('\n');
    output.push_str(&"-".repeat(title.len()).bright_blue().to_string());
    output.push('\n');
}

fn pretty_inspect_rows(info_rows: &[InspectInfoRow], rows: &[InspectRow]) -> Vec<PrettyInspectRow> {
    let mut pretty_rows = info_rows
        .iter()
        .map(|row| PrettyInspectRow {
            group: classify_info_row(row),
            label: row.name.clone(),
            value: row.value.clone(),
        })
        .collect::<Vec<_>>();

    let mut seen_ifd_0_1 = HashSet::new();
    for row in rows {
        let value = pretty_inspect_value(row);
        if matches!(row.ifd, 0 | 1)
            && !seen_ifd_0_1.insert((row.name.clone(), row.context.clone(), value.clone()))
        {
            continue;
        }

        pretty_rows.push(PrettyInspectRow {
            group: classify_exif_row(row),
            label: row.pretty_name.clone(),
            value,
        });
    }

    pretty_rows
}

fn append_nearest_location_rows(
    pretty_rows: &mut Vec<PrettyInspectRow>,
    exif: &Exif,
    warnings: &mut Vec<String>,
) {
    let Some((latitude, longitude)) = gps_coordinates(exif) else {
        return;
    };

    match nearest_locations(latitude, longitude, NEAREST_LOCATION_LIMIT) {
        Ok(locations) => {
            for (index, location) in locations.into_iter().enumerate() {
                pretty_rows.push(PrettyInspectRow {
                    group: PrettyInspectGroup::Gps,
                    label: format!("Nearest Location {}", index + 1),
                    value: format!(
                        "{}, {} ({})",
                        location.name,
                        location.country_code,
                        format_distance(location.distance_km)
                    ),
                });
            }
        }
        Err(error) => warnings.push(format!("failed to query nearest locations: {error}")),
    }
}

fn gps_coordinates(exif: &Exif) -> Option<(f64, f64)> {
    let latitude_field = exif.fields().find(|field| field.tag == Tag::GPSLatitude)?;
    let longitude_field = exif.fields().find(|field| field.tag == Tag::GPSLongitude)?;

    let latitude = decimal_gps_coordinate(latitude_field, exif)?;
    let longitude = decimal_gps_coordinate(longitude_field, exif)?;

    if latitude.is_finite() && longitude.is_finite() {
        Some((latitude, longitude))
    } else {
        None
    }
}

struct PrettyInspectRow {
    group: PrettyInspectGroup,
    label: String,
    value: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PrettyInspectGroup {
    File,
    Camera,
    Film,
    Exposure,
    Gps,
    Misc,
    Unknown,
}

impl PrettyInspectGroup {
    const OUTPUT_ORDER: [Self; 7] = [
        Self::File,
        Self::Camera,
        Self::Film,
        Self::Exposure,
        Self::Gps,
        Self::Misc,
        Self::Unknown,
    ];

    fn output_order(self) -> usize {
        match self {
            Self::File => 0,
            Self::Camera => 1,
            Self::Film => 2,
            Self::Exposure => 3,
            Self::Gps => 4,
            Self::Misc => 5,
            Self::Unknown => 6,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Camera => "Camera",
            Self::Film => "Film",
            Self::Exposure => "Exposure",
            Self::Gps => "GPS",
            Self::Misc => "MISC",
            Self::Unknown => "UNKNOWN",
        }
    }
}

fn classify_info_row(row: &InspectInfoRow) -> PrettyInspectGroup {
    if normalized_label_matches(
        &row.name,
        &[
            "filename",
            "directory",
            "filesize",
            "filemodificationdate/time",
            "fileaccessdate/time",
            "filecreationdate/time",
            "filepermissions",
            "filetype",
            "filetypeextension",
            "mimetype",
        ],
    ) {
        PrettyInspectGroup::File
    } else {
        PrettyInspectGroup::Misc
    }
}

fn classify_exif_row(row: &InspectRow) -> PrettyInspectGroup {
    if row.is_unknown {
        return PrettyInspectGroup::Unknown;
    }

    if row.context == "Gps"
        || normalized_label_starts_with(&row.name, &["gps"])
        || normalized_label_starts_with(&row.pretty_name, &["gps"])
    {
        return PrettyInspectGroup::Gps;
    }

    if is_film_label(&row.name) || is_film_label(&row.pretty_name) {
        return PrettyInspectGroup::Film;
    }

    if is_exposure_label(&row.name) || is_exposure_label(&row.pretty_name) {
        return PrettyInspectGroup::Exposure;
    }

    if is_camera_label(&row.name) || is_camera_label(&row.pretty_name) {
        return PrettyInspectGroup::Camera;
    }

    if is_file_label(&row.name) || is_file_label(&row.pretty_name) {
        return PrettyInspectGroup::File;
    }

    PrettyInspectGroup::Misc
}

fn is_file_label(label: &str) -> bool {
    let label = normalized_label(label);
    label != "filesource" && label.starts_with("file")
}

fn is_camera_label(label: &str) -> bool {
    normalized_label_matches(
        label,
        &[
            "make",
            "model",
            "filesource",
            "focallength",
            "focallengthin35mmfilm",
            "maxaperturevalue",
            "lensmake",
            "lensmodel",
        ],
    ) || normalized_label_starts_with(label, &["camera", "lens"])
}

fn is_film_label(label: &str) -> bool {
    normalized_label_matches(
        label,
        &[
            "filmroll",
            "filmmaker",
            "filmname",
            "filmformat",
            "filmcolor",
            "filmnegative",
            "filmdevelopprocess",
            "filmdeveloper",
            "filmprocesslab",
            "filmprocessdate",
            "filmscanner",
        ],
    ) || normalized_label_starts_with(label, &["analoguedata"])
}

fn is_exposure_label(label: &str) -> bool {
    normalized_label_matches(
        label,
        &[
            "exposuretime",
            "fnumber",
            "isospeedratings",
            "iso",
            "isospeed",
            "shutterspeedvalue",
            "aperturevalue",
            "brightnessvalue",
            "exposurebiasvalue",
            "exposuremode",
            "exposureprogram",
            "maxaperturevalue",
            "meteringmode",
            "photographicsensitivity",
            "sensitivitytype",
            "lightsource",
            "flash",
        ],
    )
}

fn normalized_label(label: &str) -> String {
    label
        .chars()
        .filter(|char| !char.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn normalized_label_matches(label: &str, tags: &[&str]) -> bool {
    let label = normalized_label(label);
    tags.contains(&label.as_str())
}

fn normalized_label_starts_with(label: &str, prefixes: &[&str]) -> bool {
    let label = normalized_label(label);
    prefixes.iter().any(|prefix| label.starts_with(prefix))
}

fn pretty_inspect_value(row: &InspectRow) -> String {
    if row.name == "ExposureTime" {
        return pretty_exposure_time(&row.value).unwrap_or_else(|| row.value.clone());
    }

    row.value.clone()
}

fn pretty_exposure_time(value: &str) -> Option<String> {
    let denominator = value.strip_prefix("1/")?;
    let denominator = denominator.strip_suffix(" s").unwrap_or(denominator);
    let denominator = denominator.parse::<f64>().ok()?;

    if !denominator.is_finite() {
        return None;
    }

    Some(format!("1/{:.0}", denominator.round()))
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
            format_known_field_value(field, exif, &name)
        };

        if !is_unknown {
            if let Some(decimal) = decimal_gps_coordinate(field, exif) {
                value.push_str(&format!(" ({})", format_decimal_coordinate(decimal)));
            }
        }

        Self {
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

fn format_known_field_value(field: &Field, exif: &Exif, name: &str) -> String {
    let value = field.display_value().with_unit(exif).to_string();

    if name == "ExposureTime" {
        return value
            .strip_suffix(" s")
            .map_or(value.clone(), ToString::to_string);
    }

    value
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

struct GeoLocation {
    name: String,
    country_code: String,
    latitude: f64,
    longitude: f64,
    distance_km: f64,
}

fn nearest_locations(
    latitude: f64,
    longitude: f64,
    limit: usize,
) -> Result<Vec<GeoLocation>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let connection = open_embedded_geonames_database()?;
    let mut radius_km = 25.0;
    let mut candidates = Vec::new();

    while radius_km <= 20_000.0 {
        candidates = candidate_locations(&connection, latitude, longitude, radius_km)?;

        for location in &mut candidates {
            location.distance_km =
                haversine_distance_km(latitude, longitude, location.latitude, location.longitude);
        }
        sort_locations_by_distance(&mut candidates);

        if candidates.len() >= limit && candidates[limit - 1].distance_km <= radius_km {
            break;
        }
        radius_km *= 2.0;
    }

    sort_locations_by_distance(&mut candidates);
    candidates.truncate(limit);

    Ok(candidates)
}

fn sort_locations_by_distance(locations: &mut [GeoLocation]) {
    locations.sort_by(|left, right| {
        left.distance_km
            .total_cmp(&right.distance_km)
            .then(left.country_code.cmp(&right.country_code))
            .then(left.name.cmp(&right.name))
    });
}

fn open_embedded_geonames_database() -> Result<Connection, String> {
    let mut connection = Connection::open_in_memory()
        .map_err(|error| format!("failed to open in-memory SQLite database: {error}"))?;
    let data = sqlite_owned_data(GEONAMES_DATABASE)?;

    connection
        .deserialize(DatabaseName::Main, data, true)
        .map_err(|error| format!("failed to load embedded GeoNames database: {error}"))?;

    Ok(connection)
}

fn sqlite_owned_data(bytes: &[u8]) -> Result<rusqlite::serialize::OwnedData, String> {
    let allocation_size = bytes
        .len()
        .try_into()
        .map_err(|_| "embedded GeoNames database is too large to load".to_string())?;
    let pointer = unsafe { rusqlite::ffi::sqlite3_malloc(allocation_size) };
    let pointer = NonNull::new(pointer.cast::<u8>()).ok_or_else(|| {
        "failed to allocate SQLite memory for embedded GeoNames database".to_string()
    })?;

    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer.as_ptr(), bytes.len());
        Ok(rusqlite::serialize::OwnedData::from_raw_nonnull(
            pointer,
            bytes.len(),
        ))
    }
}

fn candidate_locations(
    connection: &Connection,
    latitude: f64,
    longitude: f64,
    radius_km: f64,
) -> Result<Vec<GeoLocation>, String> {
    let latitude_delta = radius_km / 111.0;
    let longitude_delta = if latitude.abs() >= 89.0 {
        180.0
    } else {
        (radius_km / (111.0 * latitude.to_radians().cos().abs())).min(180.0)
    };
    let minimum_latitude = (latitude - latitude_delta).max(-90.0);
    let maximum_latitude = (latitude + latitude_delta).min(90.0);
    let minimum_longitude = normalize_longitude(longitude - longitude_delta);
    let maximum_longitude = normalize_longitude(longitude + longitude_delta);
    let wraps_date_line = minimum_longitude > maximum_longitude;

    let sql = if wraps_date_line {
        "
        SELECT name, country_code, latitude, longitude
        FROM locations
        WHERE latitude BETWEEN ?1 AND ?2
          AND (longitude >= ?3 OR longitude <= ?4)
        "
    } else {
        "
        SELECT name, country_code, latitude, longitude
        FROM locations
        WHERE latitude BETWEEN ?1 AND ?2
          AND longitude BETWEEN ?3 AND ?4
        "
    };

    let mut statement = connection
        .prepare(sql)
        .map_err(|error| format!("failed to prepare GeoNames query: {error}"))?;
    let rows = statement
        .query_map(
            params![
                minimum_latitude,
                maximum_latitude,
                minimum_longitude,
                maximum_longitude
            ],
            |row| {
                Ok(GeoLocation {
                    name: row.get(0)?,
                    country_code: row.get(1)?,
                    latitude: row.get(2)?,
                    longitude: row.get(3)?,
                    distance_km: 0.0,
                })
            },
        )
        .map_err(|error| format!("failed to query GeoNames database: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read GeoNames row: {error}"))
}

fn normalize_longitude(longitude: f64) -> f64 {
    let mut normalized = longitude;
    while normalized < -180.0 {
        normalized += 360.0;
    }
    while normalized > 180.0 {
        normalized -= 360.0;
    }
    normalized
}

fn haversine_distance_km(
    latitude: f64,
    longitude: f64,
    other_latitude: f64,
    other_longitude: f64,
) -> f64 {
    let latitude_delta = (other_latitude - latitude).to_radians();
    let longitude_delta = (other_longitude - longitude).to_radians();
    let latitude = latitude.to_radians();
    let other_latitude = other_latitude.to_radians();
    let a = (latitude_delta / 2.0).sin().powi(2)
        + latitude.cos() * other_latitude.cos() * (longitude_delta / 2.0).sin().powi(2);

    2.0 * EARTH_RADIUS_KM * a.sqrt().asin()
}

fn format_distance(distance_km: f64) -> String {
    if distance_km < 1.0 {
        format!("{:.0} m", distance_km * 1_000.0)
    } else {
        format!("{distance_km:.1} km")
    }
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
    fn formats_empty_pretty_inspect_output() {
        colored::control::set_override(true);

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
            "\u{1b}[33m<No EXIF metadata found>\u{1b}[0m"
        );
    }

    #[test]
    fn formats_empty_raw_inspect_output() {
        assert_eq!(
            format_inspect_output(
                Path::new("image.tif"),
                &InspectMetadata {
                    exif: parse_raw_exif(&[]),
                    warnings: Vec::new(),
                    file_info: test_file_info(),
                },
                InspectFormat::Raw,
            ),
            "<No EXIF metadata found>"
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

        assert!(!output.contains("KNOWN"));
        assert!(!output.contains("UNKNOWN"));
        assert!(output.contains("IFD"));
        assert!(!output.contains("File Name"));
        assert!(output.contains("0x010F"));
        assert!(output.contains("0x0110"));
        assert!(output.contains("0xFDE8"));
        assert!(output.contains("Warnings:"));
        assert!(output.contains("warning: ignored malformed trailing field"));
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
        let plain_output = strip_ansi_codes(&output);

        assert!(plain_output.contains("File\n----\nFile Name  image.tif"));
        assert!(plain_output.contains("Camera\n------\nMake   \"Z\"\nModel  \"E\""));
        assert!(plain_output.contains("UNKNOWN\n-------\nUnknown Tiff Tag  Short([42])"));
        assert!(output.contains("Warnings:"));
        assert!(output.contains("warning: ignored malformed trailing field"));
        assert!(plain_output.find("File\n").unwrap() < plain_output.find("Camera\n").unwrap());
        assert!(plain_output.find("Camera\n").unwrap() < plain_output.find("UNKNOWN\n").unwrap());
        assert!(!output.contains("IFD"));
        assert!(!output.contains("Tiff  "));
        assert!(!output.contains("0x010F"));
        assert!(!output.contains("0x0110"));
        assert!(!output.contains("0xFDE8"));
    }

    #[test]
    fn pretty_inspect_output_omits_empty_groups() {
        let metadata = InspectMetadata {
            exif: parse_raw_exif(&[tiff_ascii_entry(0x010f, b"Z\0")]),
            warnings: Vec::new(),
            file_info: InspectFileInfo::empty(),
        };

        let output =
            format_inspect_output(Path::new("image.tif"), &metadata, InspectFormat::Pretty);
        let plain_output = strip_ansi_codes(&output);

        assert!(plain_output.starts_with("Camera\n------\n"));
        assert!(!plain_output.contains("File\n"));
        assert!(!plain_output.contains("Film\n"));
        assert!(!plain_output.contains("Exposure\n"));
        assert!(!plain_output.contains("GPS\n"));
        assert!(!plain_output.contains("MISC\n"));
        assert!(!plain_output.contains("UNKNOWN\n"));
    }

    #[test]
    fn pretty_inspect_group_heading_is_blue_and_underlined() {
        colored::control::set_override(true);
        let mut output = String::new();

        append_pretty_group_heading(&mut output, PrettyInspectGroup::Camera);
        colored::control::set_override(false);

        assert_eq!(
            output,
            "\u{1b}[94mCamera\u{1b}[0m\n\u{1b}[94m------\u{1b}[0m\n"
        );
    }

    #[test]
    fn classifies_extra_file_info_rows_as_file() {
        for name in [
            "File Access Date/Time",
            "fileaccessdate/time",
            "File Creation Date/Time",
            "File Permissions",
            "File Type",
            "File Type Extension",
            "file type extension",
            "MIME Type",
            "MIMEType",
        ] {
            let row = InspectInfoRow::new(name, "value");

            assert!(matches!(classify_info_row(&row), PrettyInspectGroup::File));
        }
    }

    #[test]
    fn classifies_extra_camera_and_exposure_labels() {
        for label in [
            "FocalLengthIn35mmFilm",
            "Focal Length In 35mm Film",
            "focal length in 35mm film",
            "focallengthin35mmfilm",
        ] {
            assert!(is_camera_label(label));
        }

        for label in [
            "ExposureMode",
            "Exposure Mode",
            "exposure mode",
            "ExposureProgram",
            "Exposure Program",
            "exposureprogram",
            "PhotographicSensitivity",
            "Photographic Sensitivity",
            "SensitivityType",
            "Sensitivity Type",
            "sensitivity type",
        ] {
            assert!(is_exposure_label(label));
        }
    }

    #[test]
    fn normalized_label_classifiers_ignore_spacing_and_case() {
        assert!(is_file_label("file type"));
        assert!(!is_file_label("File Source"));
        assert!(is_camera_label("lens model"));
        assert!(is_camera_label("CAMERA SERIAL NUMBER"));
        assert!(is_film_label("Analogue Data Film Name"));
        assert!(is_film_label("analoguedatafilmname"));
        assert!(is_exposure_label("shutter speed value"));
        assert!(normalized_label_starts_with("G P S Latitude", &["gps"]));
    }

    #[test]
    fn pretty_inspect_output_deduplicates_identical_ifd_0_and_1_rows_only() {
        let metadata = InspectMetadata {
            exif: parse_raw_exif_with_ifd1(
                &[tiff_ascii_entry(0x010f, b"A\0")],
                &[
                    tiff_ascii_entry(0x010f, b"A\0"),
                    tiff_ascii_entry(0x0110, b"B\0"),
                ],
            ),
            warnings: Vec::new(),
            file_info: InspectFileInfo::empty(),
        };

        let output =
            format_inspect_output(Path::new("image.tif"), &metadata, InspectFormat::Pretty);

        assert_eq!(output.matches("Make").count(), 1);
        assert_eq!(output.matches("\"A\"").count(), 1);
        assert!(output.contains("Model  \"B\""));
    }

    #[test]
    fn pretty_inspect_output_keeps_ifd_0_and_1_rows_with_different_values() {
        let metadata = InspectMetadata {
            exif: parse_raw_exif_with_ifd1(
                &[tiff_ascii_entry(0x010f, b"A\0")],
                &[tiff_ascii_entry(0x010f, b"B\0")],
            ),
            warnings: Vec::new(),
            file_info: InspectFileInfo::empty(),
        };

        let output =
            format_inspect_output(Path::new("image.tif"), &metadata, InspectFormat::Pretty);

        assert_eq!(output.matches("Make").count(), 2);
        assert!(output.contains("Make  \"A\""));
        assert!(output.contains("Make  \"B\""));
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
    fn formats_exposure_time_for_pretty_output() {
        assert_eq!(
            pretty_exposure_time("1/1439.2133835330962 s"),
            Some("1/1439".to_string())
        );
        assert_eq!(
            pretty_exposure_time("1/1439.2133835330962"),
            Some("1/1439".to_string())
        );
        assert_eq!(pretty_exposure_time("1/500 s"), Some("1/500".to_string()));
        assert_eq!(pretty_exposure_time("0.5 s"), None);
    }

    #[test]
    fn raw_output_omits_exposure_time_unit() {
        let (exposure_entry, exposure_data) =
            tiff_rational_entry_with_count(0x829a, &[(1, 1439)], 200);
        let metadata = InspectMetadata {
            exif: parse_raw_exif_with_exif_entries(&[exposure_entry], &[(200, exposure_data)]),
            warnings: Vec::new(),
            file_info: InspectFileInfo::empty(),
        };

        let output = format_inspect_output(Path::new("image.tif"), &metadata, InspectFormat::Raw);

        assert!(output.contains("ExposureTime"));
        assert!(output.contains("1/1439"));
        assert!(!output.contains("1/1439 s"));
    }

    #[test]
    fn pretty_output_rounds_exposure_time_reciprocal_denominator() {
        let (exposure_entry, exposure_data) =
            tiff_rational_entry_with_count(0x829a, &[(10_000, 14_392_134)], 200);
        let metadata = InspectMetadata {
            exif: parse_raw_exif_with_exif_entries(&[exposure_entry], &[(200, exposure_data)]),
            warnings: Vec::new(),
            file_info: InspectFileInfo::empty(),
        };

        let output =
            format_inspect_output(Path::new("image.tif"), &metadata, InspectFormat::Pretty);

        assert!(output.contains("Exposure Time"));
        assert!(output.contains("1/1439"));
        assert!(!output.contains("1/1439.2134"));
        assert!(!output.contains("1/1439 s"));
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

        assert!(info_row_value(&rows, "Exifmeta Version Number").is_none());
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
        let metadata = test_gps_metadata();

        let output =
            format_inspect_output(Path::new("image.jpg"), &metadata, InspectFormat::Pretty);

        assert!(output.contains("GPS Latitude"));
        assert!(output.contains("(52.352832)"));
        assert!(output.contains("GPS Longitude"));
        assert!(output.contains("(-1.304089)"));
    }

    #[test]
    fn extracts_signed_gps_coordinates() {
        let metadata = test_gps_metadata();

        let (latitude, longitude) =
            gps_coordinates(&metadata.exif).expect("GPS coordinates should be extracted");
        assert!((latitude - 52.352832).abs() < 0.000001);
        assert!((longitude + 1.3040888).abs() < 0.000001);
    }

    #[test]
    fn pretty_inspect_output_appends_nearest_locations() {
        let metadata = test_gps_metadata();

        let output =
            format_inspect_output(Path::new("image.jpg"), &metadata, InspectFormat::Pretty);
        let plain_output = strip_ansi_codes(&output);

        assert!(plain_output.contains("GPS\n---\n"));
        assert_eq!(plain_output.matches("Nearest Location").count(), 5);
        assert!(plain_output.contains("Nearest Location 1  Dunchurch, GB (1.9 km)"));
        assert!(plain_output.contains("Nearest Location 2  Long Lawford, GB (3.2 km)"));
    }

    #[test]
    fn raw_inspect_output_omits_nearest_locations() {
        let metadata = test_gps_metadata();

        let output = format_inspect_output(Path::new("image.jpg"), &metadata, InspectFormat::Raw);

        assert!(output.contains("GPSLatitude"));
        assert!(!output.contains("Nearest Location"));
        assert!(!output.contains("Dunchurch"));
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
        assert!(!output.contains("Nearest Location"));
    }

    #[test]
    fn formats_metric_distances() {
        assert_eq!(format_distance(0.0123), "12 m");
        assert_eq!(format_distance(1.234), "1.2 km");
    }

    #[test]
    fn reads_jpeg_without_exif_as_empty_metadata() {
        colored::control::set_override(true);

        let path = temporary_test_path("no-exif.jpg");
        std::fs::write(&path, [0xff, 0xd8, 0xff, 0xd9]).expect("test JPEG should be written");

        let metadata = read_metadata(&path).expect("missing EXIF should not fail inspect");

        assert_eq!(
            format_inspect_output(&path, &metadata, InspectFormat::Pretty),
            "\u{1b}[33m<No EXIF metadata found>\u{1b}[0m"
        );
        assert_eq!(
            format_inspect_output(&path, &metadata, InspectFormat::Raw),
            "<No EXIF metadata found>"
        );

        let _ = std::fs::remove_file(path);
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

    fn test_gps_metadata() -> InspectMetadata {
        let (latitude_entry, latitude_data) =
            tiff_rational_entry(0x0002, [(52, 1), (21, 1), (101952, 10000)], 200);
        let (longitude_entry, longitude_data) =
            tiff_rational_entry(0x0004, [(1, 1), (18, 1), (1471968, 100000)], 224);

        InspectMetadata {
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
        }
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

    fn strip_ansi_codes(value: &str) -> String {
        let mut output = String::new();
        let mut chars = value.chars().peekable();

        while let Some(char) = chars.next() {
            if char == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for char in chars.by_ref() {
                    if char == 'm' {
                        break;
                    }
                }
            } else {
                output.push(char);
            }
        }

        output
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

    fn parse_raw_exif_with_ifd1(ifd0_entries: &[[u8; 12]], ifd1_entries: &[[u8; 12]]) -> Exif {
        let mut data = vec![0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(&(ifd0_entries.len() as u16).to_be_bytes());
        for entry in ifd0_entries {
            data.extend_from_slice(entry);
        }
        data.extend_from_slice(&100u32.to_be_bytes());
        data.resize(100, 0);
        data.extend_from_slice(&(ifd1_entries.len() as u16).to_be_bytes());
        for entry in ifd1_entries {
            data.extend_from_slice(entry);
        }
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

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

    fn parse_raw_exif_with_exif_entries(
        exif_entries: &[[u8; 12]],
        offset_data: &[(u32, Vec<u8>)],
    ) -> Exif {
        let mut data = vec![0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08];
        data.extend_from_slice(&1u16.to_be_bytes());
        data.extend_from_slice(&tiff_long_entry(0x8769, 100));
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        data.resize(100, 0);
        data.extend_from_slice(&(exif_entries.len() as u16).to_be_bytes());
        for entry in exif_entries {
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
        let (entry, _) = tiff_rational_entry_with_count(tag, &values, offset);
        (entry, tiff_rational_data(&values))
    }

    fn tiff_rational_entry_with_count(
        tag: u16,
        values: &[(u32, u32)],
        offset: u32,
    ) -> ([u8; 12], Vec<u8>) {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&5u16.to_be_bytes());
        entry[4..8].copy_from_slice(&(values.len() as u32).to_be_bytes());
        entry[8..12].copy_from_slice(&offset.to_be_bytes());

        (entry, tiff_rational_data(values))
    }

    fn tiff_rational_data(values: &[(u32, u32)]) -> Vec<u8> {
        let mut data = Vec::new();
        for &(numerator, denominator) in values {
            data.extend_from_slice(&numerator.to_be_bytes());
            data.extend_from_slice(&denominator.to_be_bytes());
        }

        data
    }
}
