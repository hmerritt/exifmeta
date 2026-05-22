use std::collections::HashSet;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::SystemTime;

use chrono::{DateTime, Local};
use colored::Colorize;
use exif::{Error as ExifError, Exif, Field, Reader, Tag, Value};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, DatabaseName, params, params_from_iter};
use serde_yaml::{Mapping, Value as YamlValue};

pub mod cli;
pub mod version;

pub use cli::{Cli, Command, InitArgs, InspectArgs, InspectFormat, RunArgs, ValidateArgs};

const GEONAMES_DATABASE: &[u8] = include_bytes!("../assets/geonames/cities1000.sqlite");
const NEAREST_LOCATION_LIMIT: usize = 5;
const NEAREST_CITY_MINIMUM_POPULATION: i64 = 200_000;
const EARTH_RADIUS_KM: f64 = 6_371.0088;
const METADATA_FILE_NAME: &str = "metadata.yaml";
const METADATA_YML_FILE_NAME: &str = "metadata.yml";
const METADATA_TEMPLATE: &str = r#"# ───────────────────────────────────────────────
# Metadata file for images in this directory. Used by exifmeta, https://github.com/hmerritt/exifmeta
# ───────────────────────────────────────────────

# ───────────────────────────────────────────────
# Custom Properties
# These values will not be written as EXIF, and are meant for personal organisational
# purposes — e.g. private metadata for your shoot
# ───────────────────────────────────────────────
roll: 1
date: <today>
date_end: <today>
frame_count: <image-count-in-directory>
notable_frames: []
locations: []

# ───────────────────────────────────────────────
# Global EXIF Properties
# Any valid EXIF tag can be set here. These tags will be written to ALL images.
# ───────────────────────────────────────────────
exif:
    # Camera & Lens
    Make:
    Model:
    LensMake:
    LensModel:
    FocalLength:
    MaxApertureValue:

    # Film / Capture
    ISOSpeedRatings:
    DateTimeOriginal:
    CreateDate:
    # 1 = Film Scanner
    # 2 = Reflection Print Scanner
    # 3 = Digital Camera
    FileSource: 1

    # Film
    FilmRoll:
    FilmMaker:
    FilmName:
    FilmFormat:
    FilmColor:
    FilmNegative:
    # Film Development
    FilmDevelopProcess:
    FilmDeveloper:
    FilmProcessLab:
    FilmProcessDate:
    FilmScanner:

    # Attribution
    Artist:
    Photographer:

# ───────────────────────────────────────────────
# Per Frame/File EXIF Properties
# Use this to set EXIF tags for individual files, like ExposureTime, FNumber, or
# GPS data. Values set here will override the above `exif` values.
# ───────────────────────────────────────────────
frames:
    # Frame number (first file when sorted alphabetically, useful when shooting film and files are in-order)
    1:
        - ImageDescription:
        - ExposureTime:
        - FNumber:
        # Special key (`$` prefix) that will match city/town names to GPS long/lat
        # values automatically, and set EXIF acordingly. This uses an embeded locations
        # database, no internet requried.
        - $Location:

    # Filename (direct but more verbose)
    "image-file.tif":
        - ExposureTime:
        - FNumber:
        # 0 = Unknown
        # 1 = Average
        # 2 = Center-weighted average
        # 3 = Spot
        # 4 = Multi-spot
        # 5 = Multi-segment
        # 6 = Partial
        # 255 = Other
        - MeteringMode: 2
        # Manually setting  GPS, all of the following values must be set!
        - GPSLatitude:
        - GPSLatitudeRef:
        - GPSLongitude:
        - GPSLongitudeRef:
        - GPSAltitude:
        - GPSAltitudeRef: 0 # 0 = above sea level
        - GPSMapDatum:
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    Error(String),
    Warning(String),
    Failure,
}

impl From<String> for CliError {
    fn from(error: String) -> Self {
        Self::Error(error)
    }
}

pub fn run(cli: Cli) -> Result<(), CliError> {
    version::print_title();

    match cli.command {
        Command::Run(args) => run_command(cli.dry_run, args).map_err(Into::into),
        Command::Init(args) => init_command(cli.dry_run, args),
        Command::Validate(args) => validate_command(args),
        Command::Inspect(args) => inspect_command(args).map_err(Into::into),
        Command::Interactive => stub_command(cli.dry_run, "interactive").map_err(Into::into),
        Command::Strip => stub_command(cli.dry_run, "strip").map_err(Into::into),
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
            .then_with(|| {
                pretty_inspect_row_sort_label(left).cmp(&pretty_inspect_row_sort_label(right))
            })
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
            output.push_str(&row.styled_label());
            output.push_str(&" ".repeat(name_width - row.label.len()));
            output.push_str(&format!("  {}\n", row.value));
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

fn pretty_inspect_row_sort_label(row: &PrettyInspectRow) -> String {
    if let Some(suffix) = row.label.strip_prefix("Nearest Location ") {
        format!("Nearest 0 Location {suffix}")
    } else if row.label == "Nearest City" {
        "Nearest 1 City".to_string()
    } else {
        row.label.clone()
    }
}

fn pretty_inspect_rows(info_rows: &[InspectInfoRow], rows: &[InspectRow]) -> Vec<PrettyInspectRow> {
    let mut pretty_rows = info_rows
        .iter()
        .map(|row| PrettyInspectRow {
            group: classify_info_row(row),
            label: row.name.clone(),
            label_color: None,
            value: row.value.clone(),
        })
        .collect::<Vec<_>>();

    let mut seen_ifd_0_1 = HashSet::new();
    for row in rows {
        if is_pretty_omitted_gps_reference(row) {
            continue;
        }

        let value = pretty_inspect_value(row);
        if matches!(row.ifd, 0 | 1)
            && !seen_ifd_0_1.insert((row.name.clone(), row.context.clone(), value.clone()))
        {
            continue;
        }

        pretty_rows.push(PrettyInspectRow {
            group: classify_exif_row(row),
            label: row.pretty_name.clone(),
            label_color: None,
            value,
        });
    }

    pretty_rows
}

fn is_pretty_omitted_gps_reference(row: &InspectRow) -> bool {
    matches!(row.name.as_str(), "GPSLatitudeRef" | "GPSLongitudeRef")
}

fn append_nearest_location_rows(
    pretty_rows: &mut Vec<PrettyInspectRow>,
    exif: &Exif,
    warnings: &mut Vec<String>,
) {
    let Some((latitude, longitude)) = gps_coordinates(exif) else {
        return;
    };

    match nearest_locations(latitude, longitude, NEAREST_LOCATION_LIMIT, None) {
        Ok(locations) => append_location_rows(pretty_rows, "Nearest Location", locations),
        Err(error) => warnings.push(format!("failed to query nearest locations: {error}")),
    }

    match nearest_locations(
        latitude,
        longitude,
        1,
        Some(NEAREST_CITY_MINIMUM_POPULATION),
    ) {
        Ok(locations) => append_single_location_row(pretty_rows, "Nearest City", locations),
        Err(error) => warnings.push(format!("failed to query nearest city: {error}")),
    }
}

fn append_location_rows(
    pretty_rows: &mut Vec<PrettyInspectRow>,
    label_prefix: &str,
    locations: Vec<GeoLocation>,
) {
    for (index, location) in locations.into_iter().enumerate() {
        pretty_rows.push(PrettyInspectRow {
            group: PrettyInspectGroup::Gps,
            label: format!("{} {}", label_prefix, index + 1),
            label_color: Some(PrettyLabelColor::Green),
            value: format!(
                "({}) {}, {}",
                format_distance(location.distance_km),
                location.name,
                location.country_code
            ),
        });
    }
}

fn append_single_location_row(
    pretty_rows: &mut Vec<PrettyInspectRow>,
    label: &str,
    locations: Vec<GeoLocation>,
) {
    for location in locations {
        append_location_row(pretty_rows, label, location);
    }
}

fn append_location_row(
    pretty_rows: &mut Vec<PrettyInspectRow>,
    label: &str,
    location: GeoLocation,
) {
    pretty_rows.push(PrettyInspectRow {
        group: PrettyInspectGroup::Gps,
        label: label.to_string(),
        label_color: Some(PrettyLabelColor::Green),
        value: format!(
            "({}) {}, {}",
            format_distance(location.distance_km),
            location.name,
            location.country_code
        ),
    });
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
    label_color: Option<PrettyLabelColor>,
    value: String,
}

#[derive(Clone, Copy)]
enum PrettyLabelColor {
    Green,
}

impl PrettyInspectRow {
    fn styled_label(&self) -> String {
        match self.label_color {
            Some(PrettyLabelColor::Green) => self.label.green().to_string(),
            None => self.label.clone(),
        }
    }
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
                value = format!("({}) {value}", format_decimal_coordinate(decimal));
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
    population: i64,
    #[allow(dead_code)]
    elevation: Option<i64>,
    distance_km: f64,
}

fn nearest_locations(
    latitude: f64,
    longitude: f64,
    limit: usize,
    minimum_population: Option<i64>,
) -> Result<Vec<GeoLocation>, String> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let connection = open_embedded_geonames_database()?;
    let mut radius_km = 25.0;
    let mut candidates = Vec::new();

    while radius_km <= 20_000.0 {
        candidates = candidate_locations(
            &connection,
            latitude,
            longitude,
            radius_km,
            minimum_population,
        )?;

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
            .then(left.population.cmp(&right.population))
    });
}

fn locations_by_name(connection: &Connection, name: &str) -> Result<Vec<GeoLocation>, String> {
    let mut statement = connection
        .prepare(
            "
        SELECT name, country_code, latitude, longitude, population, elevation
        FROM locations
        WHERE name = ?1 COLLATE NOCASE
        ORDER BY population DESC, country_code ASC, name ASC
        ",
        )
        .map_err(|error| format!("failed to prepare GeoNames location lookup: {error}"))?;

    let rows = statement
        .query_map(params![name], |row| {
            Ok(GeoLocation {
                name: row.get(0)?,
                country_code: row.get(1)?,
                latitude: row.get(2)?,
                longitude: row.get(3)?,
                population: row.get(4)?,
                elevation: row.get(5)?,
                distance_km: 0.0,
            })
        })
        .map_err(|error| format!("failed to query GeoNames location lookup: {error}"))?;

    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to read GeoNames location row: {error}"))
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
    minimum_population: Option<i64>,
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

    let population_filter = if minimum_population.is_some() {
        "          AND population > ?5\n"
    } else {
        ""
    };

    let sql = if wraps_date_line {
        format!(
            "
        SELECT name, country_code, latitude, longitude, population, elevation
        FROM locations
        WHERE latitude BETWEEN ?1 AND ?2
          AND (longitude >= ?3 OR longitude <= ?4)
{}
        ",
            population_filter
        )
    } else {
        format!(
            "
        SELECT name, country_code, latitude, longitude, population, elevation
        FROM locations
        WHERE latitude BETWEEN ?1 AND ?2
          AND longitude BETWEEN ?3 AND ?4
{}
        ",
            population_filter
        )
    };

    let mut statement = connection
        .prepare(&sql)
        .map_err(|error| format!("failed to prepare GeoNames query: {error}"))?;
    let mut parameters = vec![
        SqlValue::Real(minimum_latitude),
        SqlValue::Real(maximum_latitude),
        SqlValue::Real(minimum_longitude),
        SqlValue::Real(maximum_longitude),
    ];
    if let Some(minimum_population) = minimum_population {
        parameters.push(SqlValue::Integer(minimum_population));
    }
    let rows = statement
        .query_map(params_from_iter(parameters), |row| {
            Ok(GeoLocation {
                name: row.get(0)?,
                country_code: row.get(1)?,
                latitude: row.get(2)?,
                longitude: row.get(3)?,
                population: row.get(4)?,
                elevation: row.get(5)?,
                distance_km: 0.0,
            })
        })
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

fn init_command(dry_run: bool, args: InitArgs) -> Result<(), CliError> {
    let directory = args.path;

    if !directory.exists() {
        return Err(CliError::Error(format!(
            "init path does not exist: {}",
            directory.display()
        )));
    }

    if !directory.is_dir() {
        return Err(CliError::Error(format!(
            "init path is not a directory: {}",
            directory.display()
        )));
    }

    let metadata_path = directory.join(METADATA_FILE_NAME);

    if metadata_path.exists() {
        return Err(CliError::Warning(format!(
            "{} already exists: {}",
            METADATA_FILE_NAME,
            metadata_path.display()
        )));
    }

    let today = Local::now().format("%Y-%m-%d").to_string();
    let image_count = count_supported_images_in_directory(&directory)?;
    let contents = render_metadata_template(&today, image_count);

    if dry_run {
        println!(
            "init: would create {} (date={today}, frame_count={image_count})",
            metadata_path.display()
        );
        return Ok(());
    }

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&metadata_path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                CliError::Warning(format!(
                    "{} already exists: {}",
                    METADATA_FILE_NAME,
                    metadata_path.display()
                ))
            } else {
                CliError::Error(format!(
                    "failed to create {}: {error}",
                    metadata_path.display()
                ))
            }
        })?;

    file.write_all(contents.as_bytes()).map_err(|error| {
        CliError::Error(format!(
            "failed to write {}: {error}",
            metadata_path.display()
        ))
    })?;

    println!("created {}", metadata_path.display());

    Ok(())
}

fn render_metadata_template(today: &str, image_count: usize) -> String {
    METADATA_TEMPLATE
        .replace("<today>", today)
        .replace("<image-count-in-directory>", &image_count.to_string())
}

fn count_supported_images_in_directory(directory: &Path) -> Result<usize, CliError> {
    let entries = fs::read_dir(directory).map_err(|error| {
        CliError::Error(format!(
            "failed to read directory {}: {error}",
            directory.display()
        ))
    })?;

    let mut count = 0;

    for entry in entries {
        let entry = entry.map_err(|error| {
            CliError::Error(format!(
                "failed to read directory entry in {}: {error}",
                directory.display()
            ))
        })?;
        let path = entry.path();

        if path.is_file() && is_supported_image_file(&path) {
            count += 1;
        }
    }

    Ok(count)
}

fn is_supported_image_file(path: &Path) -> bool {
    detect_file_kind(path).file_type != "Unknown"
}

fn validate_command(args: ValidateArgs) -> Result<(), CliError> {
    let output = build_validate_output(args.path.as_deref());
    print_validate_output(&output);

    if output.error_count() == 0 {
        Ok(())
    } else {
        Err(CliError::Failure)
    }
}

fn build_validate_output(path: Option<&Path>) -> ValidateOutput {
    let mut output = ValidateOutput::default();
    let resolution = match resolve_metadata_path(path) {
        Ok(resolution) => resolution,
        Err(error) => {
            output.file_errors.push(error);
            return output;
        }
    };

    output.metadata_path = Some(resolution.path.clone());
    output.file_warnings = resolution.warnings;

    let contents = match fs::read_to_string(&resolution.path) {
        Ok(contents) => contents,
        Err(error) => {
            output.file_errors.push(format!(
                "failed to read {}: {error}",
                resolution.path.display()
            ));
            return output;
        }
    };

    let yaml = match serde_yaml::from_str::<YamlValue>(&contents) {
        Ok(yaml) => {
            output.yaml_ok = true;
            yaml
        }
        Err(error) => {
            output.file_errors.push(format!(
                "failed to parse {}: {error}",
                resolution.path.display()
            ));
            return output;
        }
    };

    match validate_metadata_yaml(&resolution.path, &yaml) {
        Ok(report) => {
            output.exif = Some(TagStageReport {
                tags: report.exif_tags,
                warnings: report.exif_warnings,
            });
            output.frames = Some(report.frames);
        }
        Err(error) => output.file_errors.push(error),
    }

    output
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ValidateOutput {
    metadata_path: Option<PathBuf>,
    yaml_ok: bool,
    file_warnings: Vec<String>,
    file_errors: Vec<String>,
    exif: Option<TagStageReport>,
    frames: Option<FramesStageReport>,
}

impl ValidateOutput {
    fn error_count(&self) -> usize {
        self.file_errors.len()
            + self.frames.as_ref().map_or(0, |frames| {
                frames
                    .frames
                    .iter()
                    .map(|frame| frame.errors.len())
                    .sum::<usize>()
            })
    }

    fn warning_count(&self) -> usize {
        self.file_warnings.len()
            + self.exif.as_ref().map_or(0, |report| report.warnings.len())
            + self.frames.as_ref().map_or(0, |frames| {
                frames.warnings.len()
                    + frames
                        .frames
                        .iter()
                        .map(|frame| frame.warnings.len())
                        .sum::<usize>()
            })
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TagStageReport {
    tags: TagCounts,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FramesStageReport {
    frame_number_count: usize,
    file_count: usize,
    warnings: Vec<String>,
    frames: Vec<FrameReport>,
}

fn print_validate_output(output: &ValidateOutput) {
    print!("{}", format_validate_output(output));
}

fn format_validate_output(output: &ValidateOutput) -> String {
    let mut rendered = String::new();

    append_validate_file_group(&mut rendered, output);
    append_validate_exif_group(&mut rendered, output);
    append_validate_frames_group(&mut rendered, output);
    append_validate_overview_group(&mut rendered, output);

    rendered
}

fn append_validate_file_group(output: &mut String, report: &ValidateOutput) {
    append_validate_heading(output, "file");

    if let Some(path) = &report.metadata_path {
        output.push_str(&format!(
            "metadata file: {}\n",
            format!("found {}", path.display()).green()
        ));
    } else {
        output.push_str("metadata file: skipped\n");
    }

    if report.yaml_ok {
        output.push_str(&format!("YAML format: {}\n", "ok".green()));
    } else {
        output.push_str("YAML format: skipped\n");
    }

    for warning in &report.file_warnings {
        output.push_str(&format!("{}\n", format_validate_warning(warning)));
    }
    for error in &report.file_errors {
        output.push_str(&format!("{}\n", format_validate_error(error)));
    }
}

fn append_validate_exif_group(output: &mut String, report: &ValidateOutput) {
    append_validate_heading(output, "exif");

    let Some(exif) = &report.exif else {
        output.push_str("skipped\n");
        return;
    };

    append_validate_tag_counts(output, &exif.tags);
    for warning in &exif.warnings {
        output.push_str(&format!("{}\n", format_validate_warning(warning)));
    }
}

fn append_validate_frames_group(output: &mut String, report: &ValidateOutput) {
    append_validate_heading(output, "frames");

    let Some(frames) = &report.frames else {
        output.push_str("skipped\n");
        return;
    };

    if frames.frames.is_empty() {
        output.push_str("skipped\n");
        return;
    }

    if frames.frame_number_count > 0 {
        output.push_str(&format!("frame numbers: {}\n", frames.frame_number_count));
        output.push_str(&format!("files: {}\n", frames.file_count));
        for warning in &frames.warnings {
            output.push_str(&format!("{}\n", format_validate_warning(warning)));
        }
    }

    for frame in &frames.frames {
        output.push_str(&format!(
            "{}\n",
            format!("frame {}", frame.key).bright_cyan()
        ));
        append_validate_tag_counts(output, &frame.tags);
        if let Some(file) = &frame.file {
            output.push_str(&format!("file: {}\n", file.display()));
        }
        for location_match in &frame.location_matches {
            output.push_str(&format!("{}\n", format_location_match(location_match)));
        }
        for warning in &frame.warnings {
            output.push_str(&format!("{}\n", format_validate_warning(warning)));
        }
        for error in &frame.errors {
            output.push_str(&format!("{}\n", format_validate_frame_error(error)));
        }
    }
}

fn append_validate_overview_group(output: &mut String, report: &ValidateOutput) {
    append_validate_heading(output, "overview");

    let errors = report.error_count();
    let warnings = report.warning_count();
    output.push_str(&format!("errors      {errors}\n"));
    output.push_str(&format!("warnings    {warnings}\n"));

    if errors > 0 {
        output.push_str(&format!("validation: {}\n", "error".red()));
    } else if warnings > 0 {
        output.push_str(&format!(
            "validation: {} {}\n",
            "success".green(),
            "(with warnings)"
        ));
    } else {
        output.push_str(&format!("validation: {}\n", "success".green()));
    }
}

fn append_validate_heading(output: &mut String, label: &str) {
    const WIDTH: usize = 58;
    let dash_count = WIDTH.saturating_sub(label.len() + 1);
    output.push_str(&format!(
        "{}\n",
        format!("{label} {}", "─".repeat(dash_count)).bright_blue()
    ));
}

fn append_validate_tag_counts(output: &mut String, counts: &TagCounts) {
    output.push_str(&format!("standard tags: {}\n", counts.standard));
    output.push_str(&format!("unknown tags: {}\n", counts.unknown));
}

fn format_location_match(location_match: &LocationMatch) -> String {
    format!(
        "location: {} [{}, {} ({}, {})]",
        "match found".green(),
        location_match.name,
        location_match.country_code,
        format_decimal_coordinate(location_match.latitude),
        format_decimal_coordinate(location_match.longitude)
    )
}

fn format_validate_error(error: &str) -> String {
    format!("error: {error}").red().to_string()
}

fn format_validate_frame_error(error: &str) -> String {
    if is_missing_frame_file_error(error) {
        format!("file: {}", "does not exist".red())
    } else {
        format_validate_error(error)
    }
}

struct MetadataPathResolution {
    path: PathBuf,
    warnings: Vec<String>,
}

fn resolve_metadata_path(path: Option<&Path>) -> Result<MetadataPathResolution, String> {
    match path {
        Some(path) if path.is_file() => Ok(MetadataPathResolution {
            path: path.to_path_buf(),
            warnings: Vec::new(),
        }),
        Some(path) if path.is_dir() => resolve_metadata_path_in_directory(path),
        Some(path) => Err(format!("metadata path does not exist: {}", path.display())),
        None => resolve_metadata_path_in_directory(Path::new(".")),
    }
}

fn format_validate_warning(warning: &str) -> String {
    let output = format!("warning: {warning}");

    if is_location_lookup_warning(warning) {
        output.red().to_string()
    } else {
        format!("{}: {warning}", "warning".yellow())
    }
}

fn is_location_lookup_warning(warning: &str) -> bool {
    warning.contains("$Location: no match found in database")
}

fn is_missing_frame_file_error(error: &str) -> bool {
    error == "file: does not exist"
}

fn resolve_metadata_path_in_directory(directory: &Path) -> Result<MetadataPathResolution, String> {
    let yaml_path = directory.join(METADATA_FILE_NAME);
    let yml_path = directory.join(METADATA_YML_FILE_NAME);
    let yaml_exists = yaml_path.is_file();
    let yml_exists = yml_path.is_file();

    if yaml_exists {
        let mut warnings = Vec::new();
        if yml_exists {
            warnings.push(format!(
                "{} also exists and was ignored",
                yml_path.display()
            ));
        }
        return Ok(MetadataPathResolution {
            path: yaml_path,
            warnings,
        });
    }

    if yml_exists {
        return Ok(MetadataPathResolution {
            path: yml_path,
            warnings: Vec::new(),
        });
    }

    Err(format!(
        "no metadata.yaml or metadata.yml found in {}",
        directory.display()
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ValidateReport {
    exif_tags: TagCounts,
    exif_warnings: Vec<String>,
    frame_tags: TagCounts,
    location_matches: Vec<LocationMatch>,
    frames: FramesStageReport,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TagCounts {
    standard: usize,
    unknown: usize,
    unknown_names: Vec<String>,
}

fn validate_metadata_yaml(
    metadata_path: &Path,
    yaml: &YamlValue,
) -> Result<ValidateReport, String> {
    let root = yaml
        .as_mapping()
        .ok_or_else(|| "metadata YAML root must be a mapping".to_string())?;
    let mut report = ValidateReport::default();

    if let Some(exif) = yaml_mapping_get(root, "exif") {
        let exif = exif
            .as_mapping()
            .ok_or_else(|| "metadata YAML `exif` key must be a mapping".to_string())?;
        report.exif_tags = validate_tag_mapping(exif, "exif")?;
        report.exif_warnings = tag_warnings("exif", &report.exif_tags);
    }

    if let Some(frames) = yaml_mapping_get(root, "frames") {
        let frames = frames
            .as_mapping()
            .ok_or_else(|| "metadata YAML `frames` key must be a mapping".to_string())?;
        let frame_report = validate_frames_mapping(metadata_path, frames)?;
        report.frame_tags = frame_report.tags;
        report
            .location_matches
            .extend(frame_report.location_matches);
        report.frames = frame_report.frames;
        report.warnings.extend(frame_report.warnings);
    }

    Ok(report)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FrameReport {
    key: String,
    is_numeric: bool,
    file: Option<PathBuf>,
    tags: TagCounts,
    location_matches: Vec<LocationMatch>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
struct LocationMatch {
    name: String,
    country_code: String,
    latitude: f64,
    longitude: f64,
}

impl Eq for LocationMatch {}

struct FrameValidationReport {
    tags: TagCounts,
    location_matches: Vec<LocationMatch>,
    frames: FramesStageReport,
    warnings: Vec<String>,
}

fn validate_frames_mapping(
    metadata_path: &Path,
    frames: &Mapping,
) -> Result<FrameValidationReport, String> {
    let image_directory = metadata_path.parent().unwrap_or_else(|| Path::new("."));
    let image_files = supported_image_files_in_directory(image_directory)?;
    let geonames = open_embedded_geonames_database()?;
    let frame_number_count = frames
        .keys()
        .filter(|key| frame_number_from_key(key).is_some())
        .count();
    let file_count = image_files.len();
    let mut report = FrameValidationReport {
        tags: TagCounts::default(),
        location_matches: Vec::new(),
        frames: FramesStageReport {
            frame_number_count,
            file_count,
            warnings: frame_summary_warnings(frame_number_count, file_count),
            frames: Vec::new(),
        },
        warnings: Vec::new(),
    };

    for (frame_key, frame_value) in frames {
        let frame_number = frame_number_from_key(frame_key);
        let mut frame_report = FrameReport {
            key: yaml_key_label(frame_key),
            is_numeric: frame_number.is_some(),
            file: frame_number.and_then(|number| resolved_frame_file(number, &image_files)),
            ..FrameReport::default()
        };
        validate_frame_reference(
            frame_key,
            image_directory,
            &image_files,
            &mut frame_report.warnings,
            &mut frame_report.errors,
        );
        collect_frame_tags(frame_value, &geonames, &mut frame_report)?;
        frame_report
            .warnings
            .extend(tag_warnings("exif", &frame_report.tags));
        merge_tag_counts(&mut report.tags, frame_report.tags.clone());
        report
            .location_matches
            .extend(frame_report.location_matches.clone());
        report.warnings.extend(frame_report.warnings.clone());
        report.frames.frames.push(frame_report);
    }

    Ok(report)
}

fn frame_summary_warnings(frame_number_count: usize, file_count: usize) -> Vec<String> {
    if frame_number_count == 0 {
        return Vec::new();
    }

    if frame_number_count > file_count {
        vec![format!(
            "there are more frame numbers ({frame_number_count}) than image files ({file_count})"
        )]
    } else {
        Vec::new()
    }
}

fn frame_number_from_key(frame_key: &YamlValue) -> Option<usize> {
    let YamlValue::Number(number) = frame_key else {
        return None;
    };
    let number = number.as_i64()?;
    usize::try_from(number).ok().filter(|number| *number > 0)
}

fn resolved_frame_file(frame_number: usize, image_files: &[PathBuf]) -> Option<PathBuf> {
    image_files.get(frame_number.checked_sub(1)?).cloned()
}

fn validate_frame_reference(
    frame_key: &YamlValue,
    image_directory: &Path,
    image_files: &[PathBuf],
    warnings: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    match frame_key {
        YamlValue::Number(number) => {
            let Some(frame_number) = number.as_i64() else {
                warnings.push(format!(
                    "frame reference `{}` is not a valid frame number",
                    yaml_key_label(frame_key)
                ));
                return;
            };

            if frame_number < 1 || frame_number as usize > image_files.len() {
                warnings.push(format!(
                    "frame reference `{frame_number}` does not match an image file"
                ));
            }
        }
        YamlValue::String(file_name) => {
            if !image_directory.join(file_name).is_file() {
                errors.push("file: does not exist".to_string());
            }
        }
        _ => warnings.push(format!(
            "frame reference `{}` is not a valid frame key",
            yaml_key_label(frame_key)
        )),
    }
}

fn collect_frame_tags(
    frame_value: &YamlValue,
    geonames: &Connection,
    report: &mut FrameReport,
) -> Result<(), String> {
    match frame_value {
        YamlValue::Mapping(mapping) => {
            collect_frame_tag_mapping(mapping, geonames, report)?;
            Ok(())
        }
        YamlValue::Sequence(items) => {
            for item in items {
                let mapping = item
                    .as_mapping()
                    .ok_or_else(|| "metadata YAML frame entries must be mappings".to_string())?;
                if mapping.len() != 1 {
                    return Err(
                        "metadata YAML frame sequence entries must contain one tag".to_string()
                    );
                }
                collect_frame_tag_mapping(mapping, geonames, report)?;
            }
            Ok(())
        }
        _ => Err("metadata YAML frame values must be mappings or sequences".to_string()),
    }
}

fn collect_frame_tag_mapping(
    mapping: &Mapping,
    geonames: &Connection,
    report: &mut FrameReport,
) -> Result<(), String> {
    merge_tag_counts(&mut report.tags, validate_tag_mapping(mapping, "frames")?);

    for (key, value) in mapping {
        if key.as_str() == Some("$Location") {
            validate_location_value(value, geonames, report)?;
        }
    }

    Ok(())
}

fn validate_location_value(
    value: &YamlValue,
    geonames: &Connection,
    report: &mut FrameReport,
) -> Result<(), String> {
    let Some(location_name) = location_name_from_yaml(value, &mut report.warnings) else {
        return Ok(());
    };

    let locations = locations_by_name(geonames, location_name)?;
    if let Some(location) = locations.first() {
        report.location_matches.push(LocationMatch {
            name: location.name.clone(),
            country_code: location.country_code.clone(),
            latitude: location.latitude,
            longitude: location.longitude,
        });
    } else {
        report.warnings.push(format!(
            "$Location: no match found in database [for <{location_name}>]"
        ));
    }

    Ok(())
}

fn location_name_from_yaml<'a>(
    value: &'a YamlValue,
    warnings: &mut Vec<String>,
) -> Option<&'a str> {
    match value {
        YamlValue::Null => None,
        YamlValue::String(location_name) => {
            let location_name = location_name.trim();
            if location_name.is_empty() {
                None
            } else {
                Some(location_name)
            }
        }
        _ => {
            warnings.push("frames $Location value must be a string".to_string());
            None
        }
    }
}

fn validate_tag_mapping(mapping: &Mapping, context: &str) -> Result<TagCounts, String> {
    let mut counts = TagCounts::default();

    for (key, _) in mapping {
        let Some(tag_name) = key.as_str() else {
            return Err(format!(
                "metadata YAML `{context}` tag keys must be strings"
            ));
        };
        count_tag(tag_name, &mut counts);
    }

    Ok(counts)
}

fn count_tag(tag_name: &str, counts: &mut TagCounts) {
    if is_known_metadata_tag(tag_name) {
        counts.standard += 1;
    } else {
        counts.unknown += 1;
        if !counts.unknown_names.iter().any(|name| name == tag_name) {
            counts.unknown_names.push(tag_name.to_string());
        }
    }
}

fn merge_tag_counts(counts: &mut TagCounts, next: TagCounts) {
    counts.standard += next.standard;
    counts.unknown += next.unknown;
    for name in next.unknown_names {
        if !counts
            .unknown_names
            .iter()
            .any(|existing| existing == &name)
        {
            counts.unknown_names.push(name);
        }
    }
}

fn tag_warnings(context: &str, counts: &TagCounts) -> Vec<String> {
    counts
        .unknown_names
        .iter()
        .map(|name| format!("{context} tag is non-standard `{name}`"))
        .collect()
}

fn yaml_mapping_get<'a>(mapping: &'a Mapping, key: &str) -> Option<&'a YamlValue> {
    mapping.get(YamlValue::String(key.to_string()))
}

fn yaml_key_label(value: &YamlValue) -> String {
    match value {
        YamlValue::String(value) => value.clone(),
        YamlValue::Number(value) => value.to_string(),
        YamlValue::Bool(value) => value.to_string(),
        _ => format!("{value:?}"),
    }
}

fn supported_image_files_in_directory(directory: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read directory {}: {error}", directory.display()))?;
    let mut paths = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "failed to read directory entry in {}: {error}",
                directory.display()
            )
        })?;
        let path = entry.path();
        if path.is_file() && is_supported_image_file(&path) {
            paths.push(path);
        }
    }

    paths.sort_by_key(|path| file_name(path).to_ascii_lowercase());
    Ok(paths)
}

fn is_known_metadata_tag(tag_name: &str) -> bool {
    tag_name.starts_with('$') && matches!(tag_name, "$Location")
        || STANDARD_EXIF_TAG_NAMES.contains(&tag_name)
}

const STANDARD_EXIF_TAG_NAMES: &[&str] = &[
    "Acceleration",
    "ApertureValue",
    "Artist",
    "BitsPerSample",
    "BodySerialNumber",
    "BrightnessValue",
    "CFAPattern",
    "CameraElevationAngle",
    "CameraOwnerName",
    "ColorSpace",
    "ComponentsConfiguration",
    "CompressedBitsPerPixel",
    "Compression",
    "CompositeImage",
    "Contrast",
    "Copyright",
    "CreateDate",
    "CustomRendered",
    "DateTime",
    "DateTimeDigitized",
    "DateTimeOriginal",
    "DeviceSettingDescription",
    "DigitalZoomRatio",
    "ExifVersion",
    "ExposureBiasValue",
    "ExposureIndex",
    "ExposureMode",
    "ExposureProgram",
    "ExposureTime",
    "FNumber",
    "FileSource",
    "Flash",
    "FlashEnergy",
    "FlashpixVersion",
    "FocalLength",
    "FocalLengthIn35mmFilm",
    "FocalPlaneResolutionUnit",
    "FocalPlaneXResolution",
    "FocalPlaneYResolution",
    "GPSAltitude",
    "GPSAltitudeRef",
    "GPSAreaInformation",
    "GPSDateStamp",
    "GPSDestBearing",
    "GPSDestBearingRef",
    "GPSDestDistance",
    "GPSDestDistanceRef",
    "GPSDestLatitude",
    "GPSDestLatitudeRef",
    "GPSDestLongitude",
    "GPSDestLongitudeRef",
    "GPSDifferential",
    "GPSDOP",
    "GPSHPositioningError",
    "GPSImgDirection",
    "GPSImgDirectionRef",
    "GPSInfoIFDPointer",
    "GPSLatitude",
    "GPSLatitudeRef",
    "GPSLongitude",
    "GPSLongitudeRef",
    "GPSMapDatum",
    "GPSMeasureMode",
    "GPSProcessingMethod",
    "GPSSatellites",
    "GPSSpeed",
    "GPSSpeedRef",
    "GPSStatus",
    "GPSTimeStamp",
    "GPSTrack",
    "GPSTrackRef",
    "GPSVersionID",
    "GainControl",
    "Gamma",
    "Humidity",
    "ISO",
    "ISOSpeed",
    "ISOSpeedLatitudezzz",
    "ISOSpeedLatitudeyyy",
    "ISOSpeedRatings",
    "ImageDescription",
    "ImageHeight",
    "ImageLength",
    "ImageUniqueID",
    "ImageWidth",
    "InteropIFDPointer",
    "InteroperabilityIndex",
    "InteroperabilityVersion",
    "JPEGInterchangeFormat",
    "JPEGInterchangeFormatLength",
    "LensMake",
    "LensModel",
    "LensSerialNumber",
    "LensSpecification",
    "LightSource",
    "Make",
    "MakerNote",
    "MaxApertureValue",
    "MeteringMode",
    "Model",
    "OECF",
    "OffsetTime",
    "OffsetTimeDigitized",
    "OffsetTimeOriginal",
    "Orientation",
    "PhotographicSensitivity",
    "PhotometricInterpretation",
    "PixelXDimension",
    "PixelYDimension",
    "PlanarConfiguration",
    "Pressure",
    "PrimaryChromaticities",
    "RecommendedExposureIndex",
    "ReferenceBlackWhite",
    "RelatedImageFileFormat",
    "RelatedImageLength",
    "RelatedImageWidth",
    "RelatedSoundFile",
    "ResolutionUnit",
    "RowsPerStrip",
    "SamplesPerPixel",
    "Saturation",
    "SceneCaptureType",
    "SceneType",
    "SensingMethod",
    "Sharpness",
    "ShutterSpeedValue",
    "Software",
    "SourceExposureTimesOfCompositeImage",
    "SourceImageNumberOfCompositeImage",
    "SpatialFrequencyResponse",
    "SpectralSensitivity",
    "StandardOutputSensitivity",
    "StripByteCounts",
    "StripOffsets",
    "SubSecTime",
    "SubSecTimeDigitized",
    "SubSecTimeOriginal",
    "SubjectArea",
    "SubjectDistance",
    "SubjectDistanceRange",
    "SubjectLocation",
    "Temperature",
    "TileByteCounts",
    "TileOffsets",
    "TransferFunction",
    "UserComment",
    "WaterDepth",
    "WhiteBalance",
    "WhitePoint",
    "XResolution",
    "YCbCrCoefficients",
    "YCbCrPositioning",
    "YCbCrSubSampling",
    "YResolution",
];

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
    fn renders_metadata_template_values() {
        let output = render_metadata_template("2026-05-22", 12);

        assert!(output.contains("date: 2026-05-22"));
        assert!(output.contains("date_end: 2026-05-22"));
        assert!(output.contains("frame_count: 12"));
        assert!(!output.contains("<today>"));
        assert!(!output.contains("<image-count-in-directory>"));
    }

    #[test]
    fn init_creates_metadata_file() {
        let directory = temporary_test_directory("init-creates");

        init_command(
            false,
            InitArgs {
                path: directory.clone(),
            },
        )
        .expect("init should create metadata file");

        let output = std::fs::read_to_string(directory.join(METADATA_FILE_NAME))
            .expect("metadata file should be readable");
        let today = Local::now().format("%Y-%m-%d").to_string();
        assert!(output.contains(&format!("date: {today}")));
        assert!(output.contains("frame_count: 0"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn init_creates_metadata_file_in_target_directory() {
        let parent = temporary_test_directory("init-target-parent");
        let directory = parent.join("nested");
        std::fs::create_dir(&directory).expect("nested test directory should be created");

        init_command(
            false,
            InitArgs {
                path: directory.clone(),
            },
        )
        .expect("init should create metadata file in target directory");

        assert!(directory.join(METADATA_FILE_NAME).exists());
        assert!(!parent.join(METADATA_FILE_NAME).exists());

        let _ = std::fs::remove_dir_all(parent);
    }

    #[test]
    fn init_refuses_to_overwrite_existing_metadata_file() {
        let directory = temporary_test_directory("init-existing");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(&metadata, "existing").expect("metadata file should be written");

        let result = init_command(
            false,
            InitArgs {
                path: directory.clone(),
            },
        );

        assert!(
            matches!(result, Err(CliError::Warning(message)) if message.contains("metadata.yaml already exists"))
        );
        assert_eq!(
            std::fs::read_to_string(&metadata).expect("metadata file should be readable"),
            "existing"
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn init_counts_supported_images_non_recursively() {
        let directory = temporary_test_directory("init-image-count");
        let nested = directory.join("nested");
        std::fs::create_dir(&nested).expect("nested test directory should be created");
        for file_name in [
            "a.JPG", "b.jpeg", "c.jxl", "d.heif", "e.hif", "f.heic", "g.avif", "h.png", "i.tiff",
            "j.webp",
        ] {
            std::fs::write(directory.join(file_name), []).expect("test image should be written");
        }
        std::fs::write(directory.join("notes.txt"), []).expect("test text file should be written");
        std::fs::write(nested.join("nested.jpg"), []).expect("nested image should be written");

        init_command(
            false,
            InitArgs {
                path: directory.clone(),
            },
        )
        .expect("init should create metadata file");

        let output = std::fs::read_to_string(directory.join(METADATA_FILE_NAME))
            .expect("metadata file should be readable");
        assert!(output.contains("frame_count: 10"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn init_dry_run_does_not_create_metadata_file() {
        let directory = temporary_test_directory("init-dry-run");

        init_command(
            true,
            InitArgs {
                path: directory.clone(),
            },
        )
        .expect("dry-run init should succeed");

        assert!(!directory.join(METADATA_FILE_NAME).exists());

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn resolve_metadata_path_prefers_yaml_over_yml() {
        let directory = temporary_test_directory("validate-prefers-yaml");
        let yaml = directory.join(METADATA_FILE_NAME);
        let yml = directory.join(METADATA_YML_FILE_NAME);
        std::fs::write(&yaml, "exif: {}").expect("yaml metadata should be written");
        std::fs::write(&yml, "exif: {}").expect("yml metadata should be written");

        let resolution =
            resolve_metadata_path(Some(&directory)).expect("metadata path should resolve");

        assert_eq!(resolution.path, yaml);
        assert_eq!(resolution.warnings.len(), 1);
        assert!(resolution.warnings[0].contains("metadata.yml"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn resolve_metadata_path_falls_back_to_yml() {
        let directory = temporary_test_directory("validate-fallback-yml");
        let yml = directory.join(METADATA_YML_FILE_NAME);
        std::fs::write(&yml, "exif: {}").expect("yml metadata should be written");

        let resolution =
            resolve_metadata_path(Some(&directory)).expect("metadata path should resolve");

        assert_eq!(resolution.path, yml);
        assert!(resolution.warnings.is_empty());

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn resolve_metadata_path_errors_when_missing() {
        let directory = temporary_test_directory("validate-missing");

        let result = resolve_metadata_path(Some(&directory));

        assert!(matches!(result, Err(message) if message.contains("no metadata.yaml")));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_metadata_rejects_bad_structure() {
        let yaml = serde_yaml::from_str::<YamlValue>("- not\n- a mapping")
            .expect("test YAML should parse");

        let result = validate_metadata_yaml(Path::new("metadata.yaml"), &yaml);

        assert!(matches!(result, Err(message) if message.contains("root must be a mapping")));
    }

    #[test]
    fn validate_metadata_counts_standard_and_unknown_exif_tags() {
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
exif:
  Make: Nikon
  ISOSpeedRatings: 400
  CreateDate: 2026-05-22
  NotARealExifTag: value
"#,
        )
        .expect("test YAML should parse");

        let report = validate_metadata_yaml(Path::new("metadata.yaml"), &yaml)
            .expect("metadata should validate");

        assert_eq!(report.exif_tags.standard, 3);
        assert_eq!(report.exif_tags.unknown, 1);
        assert_eq!(report.exif_tags.unknown_names, ["NotARealExifTag"]);
        assert!(
            report
                .exif_warnings
                .iter()
                .any(|warning| warning == "exif tag is non-standard `NotARealExifTag`")
        );
    }

    #[test]
    fn validate_metadata_counts_frame_tags_and_location_special_key() {
        let directory = temporary_test_directory("validate-frame-tags");
        let metadata = directory.join(METADATA_FILE_NAME);
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  1:
    - ExposureTime: 1/500
    - $Location: London
    - NotARealExifTag: value
"#,
        )
        .expect("test YAML should parse");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let report = validate_metadata_yaml(&metadata, &yaml).expect("metadata should validate");

        assert_eq!(report.frame_tags.standard, 2);
        assert_eq!(report.frame_tags.unknown, 1);
        assert_eq!(report.frame_tags.unknown_names, ["NotARealExifTag"]);
        assert!(
            report
                .location_matches
                .iter()
                .any(|location_match| location_match.name == "London")
        );
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning.contains("$Location"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_metadata_warns_when_location_is_not_found() {
        let directory = temporary_test_directory("validate-missing-location");
        let metadata = directory.join(METADATA_FILE_NAME);
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  1:
    - $Location: DefinitelyNotARealPlace
"#,
        )
        .expect("test YAML should parse");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let report = validate_metadata_yaml(&metadata, &yaml).expect("metadata should validate");

        assert!(report.location_matches.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("DefinitelyNotARealPlace"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn formats_missing_location_warning_in_red() {
        colored::control::set_override(true);

        let output = format_validate_warning(
            "$Location: no match found in database [for <DefinitelyNotARealPlace>]",
        );

        assert!(output.starts_with("\u{1b}[31mwarning:"));
    }

    #[test]
    fn formats_missing_frame_file_error_without_error_label() {
        colored::control::set_override(true);

        let output = format_validate_frame_error("file: does not exist");

        assert_eq!(output, "file: \u{1b}[31mdoes not exist\u{1b}[0m");
    }

    #[test]
    fn formats_other_validate_warnings_with_yellow_label() {
        colored::control::set_override(true);

        let output = format_validate_warning("ignored metadata.yml");

        assert!(output.starts_with("\u{1b}[33mwarning\u{1b}[0m:"));
    }

    #[test]
    fn validate_metadata_ignores_blank_and_null_locations() {
        let directory = temporary_test_directory("validate-empty-locations");
        let metadata = directory.join(METADATA_FILE_NAME);
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  1:
    - $Location:
  2:
    - $Location: " "
"#,
        )
        .expect("test YAML should parse");
        std::fs::write(directory.join("one.jpg"), []).expect("test image should be written");
        std::fs::write(directory.join("two.jpg"), []).expect("test image should be written");

        let report = validate_metadata_yaml(&metadata, &yaml).expect("metadata should validate");

        assert!(report.location_matches.is_empty());
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning.contains("$Location"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_metadata_warns_when_location_value_is_not_a_string() {
        let directory = temporary_test_directory("validate-invalid-location");
        let metadata = directory.join(METADATA_FILE_NAME);
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  1:
    - $Location: [London]
"#,
        )
        .expect("test YAML should parse");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let report = validate_metadata_yaml(&metadata, &yaml).expect("metadata should validate");

        assert!(report.location_matches.is_empty());
        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("$Location value must be a string"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_metadata_warns_for_missing_frame_references() {
        let directory = temporary_test_directory("validate-missing-frames");
        let metadata = directory.join(METADATA_FILE_NAME);
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  2:
    - ExposureTime: 1/500
  "missing.tif":
    - FNumber: 2.8
"#,
        )
        .expect("test YAML should parse");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let report = validate_metadata_yaml(&metadata, &yaml).expect("metadata should validate");

        assert!(
            report
                .warnings
                .iter()
                .any(|warning| warning.contains("frame reference `2`"))
        );
        assert!(
            !report
                .warnings
                .iter()
                .any(|warning| warning == "file: does not exist")
        );
        assert!(report.frames.frames.iter().any(|frame| {
            frame
                .errors
                .iter()
                .any(|error| error == "file: does not exist")
        }));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_output_groups_are_rendered_in_order() {
        let directory = temporary_test_directory("validate-group-order");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
exif:
  Make: Nikon
frames:
  "image.jpg":
    - FNumber: 2.8
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let output = build_validate_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_validate_output(&output));

        let file = rendered.find("file ").expect("file group should render");
        let exif = rendered.find("exif ").expect("exif group should render");
        let frames = rendered
            .find("frames ")
            .expect("frames group should render");
        let overview = rendered
            .find("overview ")
            .expect("overview group should render");
        assert!(file < exif);
        assert!(exif < frames);
        assert!(frames < overview);
        assert!(rendered.contains("metadata file: found "));
        assert!(rendered.contains("YAML format: ok"));
        assert!(rendered.contains("validation: success"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_overview_renders_success_without_warnings() {
        let rendered = strip_ansi_codes(&format_validate_output(&ValidateOutput::default()));

        assert!(rendered.contains("errors      0"));
        assert!(rendered.contains("warnings    0"));
        assert!(rendered.contains("validation: success"));
        assert!(!rendered.contains("(with warnings)"));
    }

    #[test]
    fn validate_overview_renders_success_with_warnings() {
        let output = ValidateOutput {
            file_warnings: vec!["metadata.yml ignored because metadata.yaml exists".to_string()],
            ..ValidateOutput::default()
        };
        let rendered = strip_ansi_codes(&format_validate_output(&output));

        assert!(rendered.contains("errors      0"));
        assert!(rendered.contains("warnings    1"));
        assert!(rendered.contains("validation: success (with warnings)"));
    }

    #[test]
    fn validate_overview_renders_error_when_errors_exist() {
        let output = ValidateOutput {
            file_errors: vec!["failed to parse metadata.yaml".to_string()],
            file_warnings: vec!["metadata.yml ignored because metadata.yaml exists".to_string()],
            ..ValidateOutput::default()
        };
        let rendered = strip_ansi_codes(&format_validate_output(&output));

        assert!(rendered.contains("errors      1"));
        assert!(rendered.contains("warnings    1"));
        assert!(rendered.contains("validation: error"));
        assert!(!rendered.contains("validation: success"));
    }

    #[test]
    fn validate_overview_counts_missing_frame_file_as_error() {
        let directory = temporary_test_directory("validate-missing-frame-file-error");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  "missing.tif":
    - FNumber: 2.8
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("image.jpg"), []).expect("image should be written");

        let output = build_validate_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_validate_output(&output));

        assert!(rendered.contains("errors      1"));
        assert!(rendered.contains("warnings    0"));
        assert!(rendered.contains("file: does not exist"));
        assert!(rendered.contains("validation: error"));
        assert!(!rendered.contains("warning: file: does not exist"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_overview_colours_success_with_warnings() {
        colored::control::set_override(true);

        let output = ValidateOutput {
            file_warnings: vec!["metadata.yml ignored because metadata.yaml exists".to_string()],
            ..ValidateOutput::default()
        };
        let rendered = format_validate_output(&output);

        assert!(rendered.contains("validation: \u{1b}[32msuccess\u{1b}[0m"));
        assert!(rendered.contains("\u{1b}[32msuccess\u{1b}[0m (with warnings)"));
    }

    #[test]
    fn validate_output_renders_frame_blocks_in_yaml_order() {
        let directory = temporary_test_directory("validate-frame-order");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  2:
    - FNumber: 2.8
  "image.jpg":
    - ExposureTime: 1/500
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("one.jpg"), []).expect("test image should be written");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");

        let output = build_validate_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_validate_output(&output));
        let frame_two = rendered.find("frame 2").expect("frame 2 should render");
        let frame_image = rendered
            .find("frame image.jpg")
            .expect("frame image.jpg should render");

        assert!(frame_two < frame_image);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_output_prints_numeric_frame_summary_and_resolved_file() {
        let directory = temporary_test_directory("validate-numeric-frame-file");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  1:
    - FNumber: 2.8
"#,
        )
        .expect("metadata should be written");
        let first = directory.join("01.jpg");
        std::fs::write(&first, []).expect("first image should be written");

        let output = build_validate_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_validate_output(&output));

        assert!(rendered.contains("frame numbers: 1"));
        assert!(rendered.contains("files: 1"));
        let frame = rendered.find("frame 1").expect("frame should render");
        let standard_tags = rendered[frame..]
            .find("standard tags:")
            .map(|index| frame + index)
            .expect("standard tags should render");
        let unknown_tags = rendered[frame..]
            .find("unknown tags:")
            .map(|index| frame + index)
            .expect("unknown tags should render");
        let file = rendered[frame..]
            .find(&format!("file: {}", first.display()))
            .map(|index| frame + index)
            .expect("resolved frame file should render");

        assert!(frame < standard_tags);
        assert!(standard_tags < unknown_tags);
        assert!(unknown_tags < file);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_output_warns_when_numeric_frames_exceed_files() {
        let directory = temporary_test_directory("validate-more-frame-numbers");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  1:
    - FNumber: 2.8
  2:
    - ExposureTime: 1/500
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("01.jpg"), []).expect("image should be written");

        let output = build_validate_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_validate_output(&output));

        assert!(rendered.contains("frame numbers: 2"));
        assert!(rendered.contains("files: 1"));
        assert!(
            rendered.contains("warning: there are more frame numbers (2) than image files (1)")
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_output_does_not_warn_when_files_exceed_numeric_frames() {
        let directory = temporary_test_directory("validate-more-files");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  1:
    - FNumber: 2.8
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("01.jpg"), []).expect("first image should be written");
        std::fs::write(directory.join("02.jpg"), []).expect("second image should be written");

        let output = build_validate_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_validate_output(&output));

        assert!(rendered.contains("frame numbers: 1"));
        assert!(rendered.contains("files: 2"));
        assert!(!rendered.contains("warning: supported image files"));
        assert!(!rendered.contains("warning: there are more frame numbers"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_output_omits_numeric_summary_for_filename_frames() {
        let directory = temporary_test_directory("validate-filename-only");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  "image.jpg":
    - FNumber: 2.8
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("image.jpg"), []).expect("image should be written");

        let output = build_validate_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_validate_output(&output));

        assert!(!rendered.contains("frame numbers:"));
        assert!(!rendered.contains("files:"));
        assert!(!rendered.contains("\nfile: "));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_output_skips_later_groups_after_parse_error() {
        let directory = temporary_test_directory("validate-parse-error");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(&metadata, "exif: [").expect("metadata should be written");

        let output = build_validate_output(Some(&metadata));
        let rendered = strip_ansi_codes(&format_validate_output(&output));

        assert!(rendered.contains("YAML format: skipped"));
        assert!(rendered.contains("exif "));
        assert!(rendered.contains("frames "));
        assert_eq!(rendered.matches("skipped").count(), 3);
        assert!(rendered.contains("errors      1"));
        assert!(rendered.contains("validation: error"));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn validate_output_uses_expected_colours() {
        let directory = temporary_test_directory("validate-colours");
        let metadata = directory.join(METADATA_FILE_NAME);
        std::fs::write(
            &metadata,
            r#"
frames:
  "image.jpg":
    - $Location: London
"#,
        )
        .expect("metadata should be written");
        std::fs::write(directory.join("image.jpg"), []).expect("test image should be written");
        colored::control::set_override(true);

        let output = build_validate_output(Some(&metadata));
        let rendered = format_validate_output(&output);

        assert!(rendered.contains("\u{1b}[94mfile "));
        assert!(rendered.contains("\u{1b}[32mfound "));
        assert!(rendered.contains("\u{1b}[96mframe image.jpg"));
        assert!(rendered.contains("location: \u{1b}[32mmatch found\u{1b}[0m [London"));

        let _ = std::fs::remove_dir_all(directory);
    }

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
        assert!(output.contains("(52.352832) 52 deg 21 min 10.1952 sec N"));
        assert!(output.contains("GPS Longitude"));
        assert!(output.contains("(-1.304089) 1 deg 18 min 14.71968 sec W"));
        assert!(!output.contains("GPS Latitude Ref"));
        assert!(!output.contains("GPS Longitude Ref"));
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
        colored::control::set_override(true);
        let metadata = test_gps_metadata();

        let output =
            format_inspect_output(Path::new("image.jpg"), &metadata, InspectFormat::Pretty);
        colored::control::set_override(false);
        let plain_output = strip_ansi_codes(&output);

        assert!(plain_output.contains("GPS\n---\n"));
        assert_eq!(plain_output.matches("Nearest Location").count(), 5);
        assert_eq!(plain_output.matches("Nearest City").count(), 1);
        assert_eq!(plain_output.matches("Nearest Large City").count(), 0);
        assert!(plain_output.contains("Nearest Location 1  (1.9 km) Dunchurch, GB"));
        assert!(plain_output.contains("Nearest Location 2  (3.2 km) Long Lawford, GB"));
        assert!(plain_output.contains("Nearest City        (15.3 km) Coventry, GB"));
        assert!(
            plain_output
                .find("Nearest Location 5")
                .expect("nearest location rows should be present")
                < plain_output
                    .find("Nearest City")
                    .expect("nearest city rows should be present")
        );
        assert!(output.contains("\u{1b}[32mNearest Location 1\u{1b}[0m"));
        assert!(output.contains("\u{1b}[32mNearest City\u{1b}[0m"));
        assert!(!output.contains("\u{1b}[32mNearest Large City\u{1b}[0m"));
    }

    #[test]
    fn raw_inspect_output_omits_nearest_locations() {
        let metadata = test_gps_metadata();

        let output = format_inspect_output(Path::new("image.jpg"), &metadata, InspectFormat::Raw);

        assert!(output.contains("GPSLatitude"));
        assert!(output.contains("GPSLatitudeRef"));
        assert!(output.contains("GPSLongitudeRef"));
        assert!(!output.contains("Nearest Location"));
        assert!(!output.contains("Nearest City"));
        assert!(!output.contains("Nearest Large City"));
        assert!(!output.contains("Dunchurch"));
        assert!(!output.contains("Coventry"));
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
        assert!(output.contains("(10.5) 10 deg 30 min 0 sec [GPSLatitudeRef missing]"));
        assert!(!output.contains("Nearest Location"));
    }

    #[test]
    fn formats_metric_distances() {
        assert_eq!(format_distance(0.0123), "12 m");
        assert_eq!(format_distance(1.234), "1.2 km");
    }

    #[test]
    fn candidate_locations_filters_by_strict_minimum_population() {
        let connection = Connection::open_in_memory().expect("test database should open");
        connection
            .execute_batch(
                "
                CREATE TABLE locations (
                    geoname_id INTEGER NOT NULL,
                    name TEXT NOT NULL,
                    country_code TEXT NOT NULL,
                    latitude REAL NOT NULL,
                    longitude REAL NOT NULL,
                    population INTEGER NOT NULL,
                    elevation INTEGER
                );
                INSERT INTO locations VALUES
                    (1, 'Small Place', 'AA', 0.0, 0.0, 199999, NULL),
                    (2, 'Equal Place', 'AA', 0.0, 0.1, 200000, NULL),
                    (3, 'City', 'AA', 0.0, 0.2, 200001, NULL),
                    (4, 'Larger City', 'AA', 0.0, 0.3, 300000, NULL);
                ",
            )
            .expect("test locations should be inserted");

        let locations = candidate_locations(&connection, 0.0, 0.0, 50.0, Some(200_000))
            .expect("population-filtered query should succeed");

        let names = locations
            .iter()
            .map(|location| location.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["City", "Larger City"]);
        assert!(
            locations
                .iter()
                .all(|location| location.population > 200_000)
        );
    }

    #[test]
    fn locations_by_name_matches_case_insensitively() {
        let connection = test_geonames_connection();

        let locations =
            locations_by_name(&connection, "london").expect("location lookup should succeed");

        assert_eq!(locations.len(), 2);
        assert_eq!(locations[0].name, "London");
        assert_eq!(locations[0].country_code, "GB");
        assert_eq!(locations[0].latitude, 51.50853);
        assert_eq!(locations[0].longitude, -0.12574);
    }

    #[test]
    fn locations_by_name_sorts_matches_by_population_descending() {
        let connection = test_geonames_connection();

        let locations =
            locations_by_name(&connection, "London").expect("location lookup should succeed");
        let countries = locations
            .iter()
            .map(|location| location.country_code.as_str())
            .collect::<Vec<_>>();

        assert_eq!(countries, ["GB", "CA"]);
    }

    #[test]
    fn locations_by_name_returns_empty_list_when_no_match_exists() {
        let connection = test_geonames_connection();

        let locations =
            locations_by_name(&connection, "Nowhere").expect("location lookup should succeed");

        assert!(locations.is_empty());
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

    fn temporary_test_directory(name: &str) -> std::path::PathBuf {
        let path = temporary_test_path(name);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir(&path).expect("test directory should be created");
        path
    }

    fn test_geonames_connection() -> Connection {
        let connection = Connection::open_in_memory().expect("test database should open");
        connection
            .execute_batch(
                "
                CREATE TABLE locations (
                    geoname_id INTEGER NOT NULL,
                    name TEXT NOT NULL,
                    country_code TEXT NOT NULL,
                    latitude REAL NOT NULL,
                    longitude REAL NOT NULL,
                    population INTEGER NOT NULL,
                    elevation INTEGER
                );
                INSERT INTO locations VALUES
                    (1, 'London', 'GB', 51.50853, -0.12574, 8961989, 25),
                    (2, 'London', 'CA', 42.98339, -81.23304, 383822, 251),
                    (3, 'Paris', 'FR', 48.85341, 2.3488, 2138551, 42);
                ",
            )
            .expect("test locations should be inserted");
        connection
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
