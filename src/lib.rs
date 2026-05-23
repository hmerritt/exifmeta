use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::{Instant, SystemTime};

use chrono::{DateTime, Local};
use colored::Colorize;
use exif::{Error as ExifError, Exif, Field, Reader, Tag, Value};
use little_exif::exif_tag::ExifTag as WritableExifTag;
use little_exif::metadata::Metadata as WritableMetadata;
use little_exif::rational::uR64;
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
const CUSTOM_TAG_PAYLOAD_MARKER: &str = concat!("exifmeta-v", env!("CARGO_PKG_VERSION"), "\n");
const LEGACY_CUSTOM_TAG_PAYLOAD_PREFIX: &[u8] = b"exifmeta-custom-tags-v1\n";
const USER_COMMENT_ASCII_PREFIX: &[u8] = b"ASCII\0\0\0";
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
    let custom_tags = custom_tags_from_exif(&metadata.exif);
    let mut rows = metadata
        .exif
        .fields()
        .filter(|field| {
            matches!(format, InspectFormat::Raw) || !is_exifmeta_custom_payload_field(field)
        })
        .map(|field| InspectRow::from_field(field, &metadata.exif))
        .collect::<Vec<_>>();

    if rows.is_empty() && custom_tags.is_empty() {
        output.push_str(&format_empty_exif_message(format));
    } else {
        sort_inspect_rows(&mut rows);

        match format {
            InspectFormat::Pretty => append_pretty_inspect_rows(
                &mut output,
                &metadata.file_info.rows,
                &rows,
                &custom_tags,
                &metadata.exif,
                &mut warnings,
            ),
            InspectFormat::Raw => append_raw_inspect_rows(&mut output, &rows),
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
    custom_tags: &[CustomTag],
    exif: &Exif,
    warnings: &mut Vec<String>,
) {
    let mut pretty_rows = pretty_inspect_rows(info_rows, rows, custom_tags);
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

        append_validate_heading(output, group.label());

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

fn pretty_inspect_row_sort_label(row: &PrettyInspectRow) -> String {
    if let Some(suffix) = row.label.strip_prefix("GPS Nearest Location ") {
        format!("GPS Nearest 0 Location {suffix}")
    } else if row.label == "GPS Nearest City" {
        "GPS Nearest 1 City".to_string()
    } else {
        row.label.clone()
    }
}

fn pretty_inspect_rows(
    info_rows: &[InspectInfoRow],
    rows: &[InspectRow],
    custom_tags: &[CustomTag],
) -> Vec<PrettyInspectRow> {
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

    for tag in custom_tags {
        pretty_rows.push(PrettyInspectRow {
            group: PrettyInspectGroup::Custom,
            label: title_case_tag_name(&tag.name),
            label_color: None,
            value: custom_tag_value_label(&tag.value),
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

    let nearest_location_rows =
        match nearest_locations(latitude, longitude, NEAREST_LOCATION_LIMIT, None) {
            Ok(locations) => locations,
            Err(error) => {
                warnings.push(format!("failed to query nearest locations: {error}"));
                Vec::new()
            }
        };
    append_location_rows(pretty_rows, "GPS Nearest Location", &nearest_location_rows);

    match nearest_locations(
        latitude,
        longitude,
        1,
        Some(NEAREST_CITY_MINIMUM_POPULATION),
    ) {
        Ok(locations) => append_non_duplicate_location_rows(
            pretty_rows,
            "GPS Nearest City",
            locations,
            &nearest_location_rows,
        ),
        Err(error) => warnings.push(format!("failed to query nearest city: {error}")),
    }
}

fn append_location_rows(
    pretty_rows: &mut Vec<PrettyInspectRow>,
    label_prefix: &str,
    locations: &[GeoLocation],
) {
    for (index, location) in locations.iter().enumerate() {
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

fn append_non_duplicate_location_rows(
    pretty_rows: &mut Vec<PrettyInspectRow>,
    label: &str,
    locations: Vec<GeoLocation>,
    existing_locations: &[GeoLocation],
) {
    for location in locations {
        if existing_locations
            .iter()
            .all(|existing| !is_same_geo_location(existing, &location))
        {
            append_location_row(pretty_rows, label, &location);
        }
    }
}

fn append_location_row(
    pretty_rows: &mut Vec<PrettyInspectRow>,
    label: &str,
    location: &GeoLocation,
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

fn is_same_geo_location(left: &GeoLocation, right: &GeoLocation) -> bool {
    left.name == right.name
        && left.country_code == right.country_code
        && left.latitude.to_bits() == right.latitude.to_bits()
        && left.longitude.to_bits() == right.longitude.to_bits()
        && left.population == right.population
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
    Custom,
    Exposure,
    Gps,
    Misc,
    Unknown,
}

impl PrettyInspectGroup {
    const OUTPUT_ORDER: [Self; 8] = [
        Self::File,
        Self::Camera,
        Self::Film,
        Self::Exposure,
        Self::Gps,
        Self::Custom,
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
            Self::Custom => 5,
            Self::Misc => 6,
            Self::Unknown => 7,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Camera => "camera",
            Self::Film => "film",
            Self::Custom => "custom",
            Self::Exposure => "exposure",
            Self::Gps => "gps",
            Self::Misc => "misc",
            Self::Unknown => "unknown",
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
    if name == "UserComment" {
        if let Some(value) = visible_user_comment_text(field) {
            return value;
        }
    }

    let value = field.display_value().with_unit(exif).to_string();

    if name == "ExposureTime" {
        return value
            .strip_suffix(" s")
            .map_or(value.clone(), ToString::to_string);
    }

    value
}

fn visible_user_comment_text(field: &Field) -> Option<String> {
    let bytes = user_comment_bytes(field)?;
    let body = bytes
        .strip_prefix(USER_COMMENT_ASCII_PREFIX)
        .unwrap_or(bytes);
    std::str::from_utf8(body).ok().map(ToString::to_string)
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

#[derive(Debug, Clone, PartialEq)]
struct CustomTag {
    name: String,
    value: YamlValue,
}

fn custom_tags_from_exif(exif: &Exif) -> Vec<CustomTag> {
    exif.fields()
        .filter(|field| field.tag == Tag::UserComment)
        .find_map(custom_tags_from_field)
        .unwrap_or_default()
}

fn custom_tags_from_field(field: &Field) -> Option<Vec<CustomTag>> {
    custom_tags_from_bytes(user_comment_bytes(field)?)
}

fn user_comment_bytes(field: &Field) -> Option<&[u8]> {
    match &field.value {
        Value::Undefined(bytes, _) => Some(bytes.as_slice()),
        Value::Ascii(values) => values.first().map(Vec::as_slice),
        _ => None,
    }
}

fn is_exifmeta_custom_payload_field(field: &Field) -> bool {
    field.tag == Tag::UserComment
        && user_comment_bytes(field).is_some_and(|bytes| custom_tags_from_bytes(bytes).is_some())
}

fn custom_tags_from_bytes(bytes: &[u8]) -> Option<Vec<CustomTag>> {
    let body = if let Some(body) = bytes.strip_prefix(LEGACY_CUSTOM_TAG_PAYLOAD_PREFIX) {
        return custom_tags_from_yaml_bytes(body);
    } else if let Some(body) = bytes.strip_prefix(USER_COMMENT_ASCII_PREFIX) {
        body
    } else {
        bytes
    };

    custom_tags_from_json_bytes(body)
}

fn custom_tag_json_body(bytes: &[u8]) -> &[u8] {
    bytes
        .strip_prefix(CUSTOM_TAG_PAYLOAD_MARKER.as_bytes())
        .unwrap_or(bytes)
}

fn custom_tags_from_yaml_bytes(bytes: &[u8]) -> Option<Vec<CustomTag>> {
    let mapping = serde_yaml::from_slice::<Mapping>(bytes).ok()?;
    custom_tags_from_mapping(mapping)
}

fn custom_tags_from_json_bytes(bytes: &[u8]) -> Option<Vec<CustomTag>> {
    let mapping = serde_json::from_slice::<Mapping>(custom_tag_json_body(bytes)).ok()?;
    custom_tags_from_mapping(mapping)
}

fn custom_tags_from_mapping(mapping: Mapping) -> Option<Vec<CustomTag>> {
    let mut tags = Vec::new();

    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            continue;
        };
        tags.push(CustomTag {
            name: name.to_string(),
            value,
        });
    }

    if tags.is_empty() { None } else { Some(tags) }
}

fn encode_custom_tags(tags: &[CustomTag]) -> Result<Vec<u8>, String> {
    let mut mapping = Mapping::new();
    for tag in tags {
        mapping.insert(YamlValue::String(tag.name.clone()), tag.value.clone());
    }

    let mut bytes = USER_COMMENT_ASCII_PREFIX.to_vec();
    bytes.extend_from_slice(CUSTOM_TAG_PAYLOAD_MARKER.as_bytes());
    let body = serde_json::to_string(&mapping)
        .map_err(|error| format!("failed to encode custom tags: {error}"))?;
    bytes.extend_from_slice(body.as_bytes());
    Ok(bytes)
}

fn custom_tag_value_label(value: &YamlValue) -> String {
    match value {
        YamlValue::Null => "<null>".to_string(),
        YamlValue::Bool(value) => value.to_string(),
        YamlValue::Number(value) => value.to_string(),
        YamlValue::String(value) => value.clone(),
        YamlValue::Sequence(_) | YamlValue::Mapping(_) => serde_yaml::to_string(value)
            .map(|value| value.trim().replace('\n', " "))
            .unwrap_or_else(|_| format!("{value:?}")),
        YamlValue::Tagged(_) => format!("{value:?}"),
    }
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
    let mut first_group = true;

    append_validate_file_group(&mut rendered, &mut first_group, output);
    append_validate_exif_group(&mut rendered, &mut first_group, output);
    append_validate_frames_group(&mut rendered, &mut first_group, output);
    append_validate_overview_group(&mut rendered, &mut first_group, output);

    rendered
}

fn append_validate_file_group(
    output: &mut String,
    first_group: &mut bool,
    report: &ValidateOutput,
) {
    append_spaced_validate_heading(output, first_group, "file");

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

fn append_validate_exif_group(
    output: &mut String,
    first_group: &mut bool,
    report: &ValidateOutput,
) {
    append_spaced_validate_heading(output, first_group, "exif");

    let Some(exif) = &report.exif else {
        output.push_str("skipped\n");
        return;
    };

    append_validate_tag_counts(output, &exif.tags);
    for warning in &exif.warnings {
        output.push_str(&format!("{}\n", format_validate_warning(warning)));
    }
}

fn append_validate_frames_group(
    output: &mut String,
    first_group: &mut bool,
    report: &ValidateOutput,
) {
    append_spaced_validate_heading(output, first_group, "frames");

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

fn append_validate_overview_group(
    output: &mut String,
    first_group: &mut bool,
    report: &ValidateOutput,
) {
    append_spaced_validate_heading(output, first_group, "overview");

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
    const WIDTH: usize = 50;
    let dash_count = WIDTH.saturating_sub(label.len() + 1);
    output.push_str(&format!(
        "{}\n",
        format!("{label} {}", "─".repeat(dash_count)).bright_blue()
    ));
}

fn append_spaced_validate_heading(output: &mut String, first_group: &mut bool, label: &str) {
    if *first_group {
        *first_group = false;
    } else {
        output.push('\n');
    }
    append_validate_heading(output, label);
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
    let image_directory = path_parent_or_current(metadata_path);
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
    let started = Instant::now();
    let request = RunRequest::from_args(&args);
    let resolution = resolve_metadata_path(request.metadata.as_deref())?;

    let metadata_dir = path_parent_or_current(&resolution.path);
    let targets = resolve_run_targets(
        metadata_dir,
        request.targets.as_deref(),
        args.recursive,
        &args.extensions,
    )?;

    if targets.is_empty() {
        return Err("no target images matched".to_string());
    }

    let contents = fs::read_to_string(&resolution.path)
        .map_err(|error| format!("failed to read {}: {error}", resolution.path.display()))?;
    let yaml = serde_yaml::from_str::<YamlValue>(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", resolution.path.display()))?;
    validate_metadata_yaml(&resolution.path, &yaml)?;

    let plan = build_run_plan(&resolution.path, &yaml, &targets)?;
    let mut summary = RunSummary::default();
    let mut output = RunOutput {
        metadata_path: resolution.path.clone(),
        dry_run,
        target_count: targets.len(),
        file_warnings: resolution.warnings,
        files: Vec::new(),
        skipped_files: Vec::new(),
    };
    summary.warnings += output.file_warnings.len();

    for image in targets {
        let Some(frame) = plan.frame_for_image(&image) else {
            summary.skipped_files += 1;
            output.skipped_files.push(image);
            continue;
        };

        let result = apply_tags_to_image(&image, &frame.tags, dry_run, &args);
        summary.add(&result);
        output.files.push(RunFileOutput {
            label: frame.label,
            image,
            result,
        });
    }

    summary.elapsed_ms = started.elapsed().as_millis();
    print!("{}", format_run_output(&output, &summary));

    if summary.errors > 0 {
        Err("one or more target images failed".to_string())
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct RunRequest {
    metadata: Option<PathBuf>,
    targets: Option<String>,
}

impl RunRequest {
    fn from_args(args: &RunArgs) -> Self {
        match (&args.metadata_or_targets, &args.targets) {
            (Some(metadata), Some(targets)) => Self {
                metadata: Some(metadata.clone()),
                targets: Some(targets.clone()),
            },
            (Some(single), None) if looks_like_metadata_path(single) => Self {
                metadata: Some(single.clone()),
                targets: None,
            },
            (Some(single), None) => Self {
                metadata: None,
                targets: Some(path_to_pattern(single)),
            },
            (None, Some(targets)) => Self {
                metadata: None,
                targets: Some(targets.clone()),
            },
            (None, None) => Self::default(),
        }
    }
}

fn looks_like_metadata_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(extension) if extension.eq_ignore_ascii_case("yaml") || extension.eq_ignore_ascii_case("yml")
    ) || path.is_dir()
}

fn path_to_pattern(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn path_parent_or_current(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn resolve_run_targets(
    metadata_dir: &Path,
    target_pattern: Option<&str>,
    recursive: bool,
    extensions: &[String],
) -> Result<Vec<PathBuf>, String> {
    let allowed_extensions = normalized_extensions(extensions);
    let mut targets = if let Some(pattern) = target_pattern {
        resolve_explicit_targets(metadata_dir, pattern)?
    } else {
        supported_image_files(metadata_dir, recursive)?
    };

    targets.retain(|path| {
        is_supported_image_file(path)
            && (allowed_extensions.is_empty()
                || file_extension(path)
                    .is_some_and(|extension| allowed_extensions.contains(&extension)))
    });
    targets.sort_by_key(|path| path_to_pattern(path));
    targets.dedup();
    Ok(targets)
}

fn normalized_extensions(extensions: &[String]) -> HashSet<String> {
    extensions
        .iter()
        .filter_map(|extension| {
            let normalized = extension
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase();
            (!normalized.is_empty()).then_some(normalized)
        })
        .collect()
}

fn resolve_explicit_targets(metadata_dir: &Path, pattern: &str) -> Result<Vec<PathBuf>, String> {
    let pattern = pattern.trim();
    if pattern.is_empty() {
        return Ok(Vec::new());
    }

    let candidate = PathBuf::from(pattern);
    let absolute_candidate = if candidate.is_absolute() {
        candidate.clone()
    } else {
        metadata_dir.join(&candidate)
    };

    if !contains_glob(pattern) {
        if absolute_candidate.is_dir() {
            return supported_image_files(&absolute_candidate, false);
        }
        return Ok(absolute_candidate
            .is_file()
            .then_some(absolute_candidate)
            .into_iter()
            .collect());
    }

    let recursive = pattern.split(['/', '\\']).any(|part| part == "**");
    let candidates = supported_image_files(metadata_dir, recursive)?;
    let normalized_pattern = pattern.replace('\\', "/");

    Ok(candidates
        .into_iter()
        .filter(|path| {
            let subject = if Path::new(pattern).is_absolute() {
                path_to_pattern(path)
            } else {
                path.strip_prefix(metadata_dir)
                    .map(path_to_pattern)
                    .unwrap_or_else(|_| path_to_pattern(path))
            };
            glob_matches(&normalized_pattern, &subject)
        })
        .collect())
}

fn contains_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

fn supported_image_files(directory: &Path, recursive: bool) -> Result<Vec<PathBuf>, String> {
    let mut paths = Vec::new();
    collect_supported_image_files(directory, recursive, &mut paths)?;
    paths.sort_by_key(|path| file_name(path).to_ascii_lowercase());
    Ok(paths)
}

fn collect_supported_image_files(
    directory: &Path,
    recursive: bool,
    paths: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("failed to read directory {}: {error}", directory.display()))?;

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
        } else if recursive && path.is_dir() {
            collect_supported_image_files(&path, recursive, paths)?;
        }
    }

    Ok(())
}

fn glob_matches(pattern: &str, subject: &str) -> bool {
    let pattern_parts = pattern.split('/').collect::<Vec<_>>();
    let subject_parts = subject.split('/').collect::<Vec<_>>();
    glob_parts_match(&pattern_parts, &subject_parts)
}

fn glob_parts_match(pattern: &[&str], subject: &[&str]) -> bool {
    if pattern.is_empty() {
        return subject.is_empty();
    }

    if pattern[0] == "**" {
        return glob_parts_match(&pattern[1..], subject)
            || (!subject.is_empty() && glob_parts_match(pattern, &subject[1..]));
    }

    !subject.is_empty()
        && wildcard_match(pattern[0].as_bytes(), subject[0].as_bytes())
        && glob_parts_match(&pattern[1..], &subject[1..])
}

fn wildcard_match(pattern: &[u8], subject: &[u8]) -> bool {
    if pattern.is_empty() {
        return subject.is_empty();
    }

    match pattern[0] {
        b'*' => {
            wildcard_match(&pattern[1..], subject)
                || (!subject.is_empty() && wildcard_match(pattern, &subject[1..]))
        }
        b'?' => !subject.is_empty() && wildcard_match(&pattern[1..], &subject[1..]),
        byte => {
            !subject.is_empty()
                && byte.eq_ignore_ascii_case(&subject[0])
                && wildcard_match(&pattern[1..], &subject[1..])
        }
    }
}

#[derive(Debug, Clone)]
struct RunTag {
    name: String,
    value: YamlValue,
}

#[derive(Debug, Clone, Default)]
struct RunPlan {
    global: Vec<RunTag>,
    frames: BTreeMap<PathBuf, RunFramePlan>,
}

impl RunPlan {
    fn frame_for_image(&self, image: &Path) -> Option<RunFramePlan> {
        let frame = self.frames.get(image);
        if self.global.is_empty() && frame.is_none_or(|frame| frame.tags.is_empty()) {
            return None;
        }

        let mut merged = self.global.clone();
        if let Some(frame) = frame {
            for tag in &frame.tags {
                if let Some(existing) = merged.iter_mut().find(|existing| existing.name == tag.name)
                {
                    *existing = tag.clone();
                } else {
                    merged.push(tag.clone());
                }
            }
        }
        normalize_iso_aliases(&mut merged);
        Some(RunFramePlan {
            label: frame
                .map(|frame| frame.label.clone())
                .unwrap_or_else(|| run_file_heading(image)),
            tags: merged,
        })
    }
}

#[derive(Debug, Clone, Default)]
struct RunFramePlan {
    label: String,
    tags: Vec<RunTag>,
}

fn normalize_iso_aliases(tags: &mut Vec<RunTag>) {
    let Some(value) = tags
        .iter()
        .rev()
        .find(|tag| is_iso_alias(&tag.name))
        .map(|tag| tag.value.clone())
    else {
        return;
    };

    tags.retain(|tag| !is_iso_alias(&tag.name));
    for name in ["ISO", "ISOSpeed", "ISOSpeedRatings"] {
        tags.push(RunTag {
            name: name.to_string(),
            value: value.clone(),
        });
    }
}

fn is_iso_alias(name: &str) -> bool {
    matches!(name, "ISO" | "ISOSpeed" | "ISOSpeedRatings")
}

fn build_run_plan(
    metadata_path: &Path,
    yaml: &YamlValue,
    targets: &[PathBuf],
) -> Result<RunPlan, String> {
    let root = yaml
        .as_mapping()
        .ok_or_else(|| "metadata YAML root must be a mapping".to_string())?;
    let mut plan = RunPlan::default();

    if let Some(exif) = yaml_mapping_get(root, "exif") {
        plan.global = collect_run_tags_from_mapping(
            exif.as_mapping()
                .ok_or_else(|| "metadata YAML `exif` key must be a mapping".to_string())?,
        )?;
    }

    if let Some(frames) = yaml_mapping_get(root, "frames") {
        let frames = frames
            .as_mapping()
            .ok_or_else(|| "metadata YAML `frames` key must be a mapping".to_string())?;
        let image_directory = path_parent_or_current(metadata_path);
        for (frame_key, frame_value) in frames {
            let Some(image) = resolve_run_frame_target(frame_key, image_directory, targets) else {
                continue;
            };
            plan.frames.insert(
                image.clone(),
                RunFramePlan {
                    label: run_frame_label(frame_key, &image),
                    tags: collect_run_tags_from_frame_value(frame_value)?,
                },
            );
        }
    }

    Ok(plan)
}

fn run_frame_label(frame_key: &YamlValue, image: &Path) -> String {
    if let Some(frame_number) = frame_number_from_key(frame_key) {
        return format!("frame {frame_number} ({})", run_file_heading(image));
    }

    run_file_heading(image)
}

fn resolve_run_frame_target(
    frame_key: &YamlValue,
    image_directory: &Path,
    targets: &[PathBuf],
) -> Option<PathBuf> {
    if let Some(frame_number) = frame_number_from_key(frame_key) {
        return targets.get(frame_number.checked_sub(1)?).cloned();
    }

    let file_name = frame_key.as_str()?;
    let absolute = image_directory.join(file_name);
    targets.iter().find(|target| **target == absolute).cloned()
}

fn collect_run_tags_from_frame_value(value: &YamlValue) -> Result<Vec<RunTag>, String> {
    match value {
        YamlValue::Mapping(mapping) => collect_run_tags_from_mapping(mapping),
        YamlValue::Sequence(items) => {
            let mut tags = Vec::new();
            for item in items {
                let mapping = item
                    .as_mapping()
                    .ok_or_else(|| "metadata YAML frame entries must be mappings".to_string())?;
                if mapping.len() != 1 {
                    return Err(
                        "metadata YAML frame sequence entries must contain one tag".to_string()
                    );
                }
                tags.extend(collect_run_tags_from_mapping(mapping)?);
            }
            Ok(tags)
        }
        _ => Err("metadata YAML frame values must be mappings or sequences".to_string()),
    }
}

fn collect_run_tags_from_mapping(mapping: &Mapping) -> Result<Vec<RunTag>, String> {
    let mut tags = Vec::new();
    for (key, value) in mapping {
        let Some(name) = key.as_str() else {
            return Err("metadata YAML tag keys must be strings".to_string());
        };
        tags.push(RunTag {
            name: name.to_string(),
            value: value.clone(),
        });
    }
    Ok(tags)
}

#[derive(Debug, Clone, Default)]
struct RunFileResult {
    written: usize,
    skipped: Vec<String>,
    warnings: Vec<String>,
    errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct RunSummary {
    written_tags: usize,
    skipped_tags: usize,
    skipped_files: usize,
    warnings: usize,
    errors: usize,
    elapsed_ms: u128,
}

impl RunSummary {
    fn add(&mut self, result: &RunFileResult) {
        self.written_tags += result.written;
        self.skipped_tags += result.skipped.len();
        self.warnings += result.warnings.len() + result.skipped.len();
        self.errors += result.errors.len();
    }
}

#[derive(Debug, Clone)]
struct RunOutput {
    metadata_path: PathBuf,
    dry_run: bool,
    target_count: usize,
    file_warnings: Vec<String>,
    files: Vec<RunFileOutput>,
    skipped_files: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct RunFileOutput {
    label: String,
    image: PathBuf,
    result: RunFileResult,
}

fn apply_tags_to_image(
    image: &Path,
    tags: &[RunTag],
    dry_run: bool,
    args: &RunArgs,
) -> RunFileResult {
    let mut result = RunFileResult::default();
    let mut writable_tags = Vec::new();
    let mut custom_tags = Vec::new();

    for tag in tags {
        if is_blank_yaml_value(&tag.value) {
            continue;
        }

        if tag.name == "$Location" {
            match expand_location_tag(&tag.value) {
                Ok(expanded) => writable_tags.extend(expanded),
                Err(RunTagError::Ignored) => {}
                Err(RunTagError::Warning(message)) => result.warnings.push(message),
            }
        } else if let Some(tag) = writable_exif_tag(&tag.name, &tag.value) {
            writable_tags.push(tag);
        } else {
            custom_tags.push(CustomTag {
                name: tag.name.clone(),
                value: tag.value.clone(),
            });
        }
    }
    dedupe_writable_tags(&mut writable_tags);
    if !custom_tags.is_empty() {
        match encode_custom_tags(&custom_tags) {
            Ok(payload) => writable_tags.push(WritableExifTag::UserComment(payload)),
            Err(error) => result.errors.push(error),
        }
    }

    if dry_run {
        result.written = writable_tags
            .iter()
            .filter(|tag| !matches!(tag, WritableExifTag::UserComment(_)))
            .count()
            + custom_tags.len();
        return result;
    }

    if args.strip {
        if let Err(error) = WritableMetadata::file_clear_metadata(image) {
            result
                .errors
                .push(format!("failed to strip EXIF metadata: {error}"));
            return result;
        }
    }

    let mut metadata =
        WritableMetadata::new_from_path(image).unwrap_or_else(|_| WritableMetadata::new());
    if !custom_tags.is_empty()
        && metadata
            .get_tag(&WritableExifTag::UserComment(Vec::new()))
            .any(|tag| !writable_user_comment_has_custom_payload(tag))
    {
        result
            .warnings
            .push("replacing existing non-exifmeta UserComment".to_string());
    }

    for tag in writable_tags {
        let is_custom_payload = matches!(tag, WritableExifTag::UserComment(_));
        if args.no_overwrite && !is_custom_payload && metadata.get_tag(&tag).next().is_some() {
            result.skipped.push(format!(
                "{} already exists",
                writable_tag_name(&tag).unwrap_or("EXIF tag")
            ));
            continue;
        }

        metadata.set_tag(tag);
        result.written += if is_custom_payload {
            custom_tags.len()
        } else {
            1
        };
    }

    if result.written > 0 {
        if let Err(error) = metadata.write_to_file(image) {
            result
                .errors
                .push(format!("failed to write EXIF metadata: {error}"));
        }
    }

    result
}

fn writable_user_comment_has_custom_payload(tag: &WritableExifTag) -> bool {
    matches!(tag, WritableExifTag::UserComment(bytes) if custom_tags_from_bytes(bytes).is_some())
}

fn dedupe_writable_tags(tags: &mut Vec<WritableExifTag>) {
    let mut deduped: Vec<WritableExifTag> = Vec::new();
    for tag in tags.drain(..) {
        if let Some(existing) = deduped
            .iter_mut()
            .find(|existing| writable_tag_identity(existing) == writable_tag_identity(&tag))
        {
            *existing = tag;
        } else {
            deduped.push(tag);
        }
    }
    *tags = deduped;
}

fn writable_tag_identity(tag: &WritableExifTag) -> (u16, String) {
    (tag.as_u16(), format!("{:?}", tag.get_group()))
}

fn format_run_output(output: &RunOutput, summary: &RunSummary) -> String {
    let mut rendered = String::new();
    let mut first_group = true;

    append_run_metadata_group(&mut rendered, &mut first_group, output);
    append_run_frames_group(&mut rendered, &mut first_group, output);
    append_run_overview_group(&mut rendered, &mut first_group, summary);

    rendered
}

fn append_run_metadata_group(rendered: &mut String, first_group: &mut bool, output: &RunOutput) {
    append_spaced_validate_heading(rendered, first_group, "run");
    rendered.push_str(&format!(
        "metadata file: {}\n",
        output.metadata_path.display()
    ));
    rendered.push_str(&format!("targets: {}\n", output.target_count));
    if output.dry_run {
        rendered.push_str(&format!("mode: {}\n", "dry-run".yellow()));
    }
    for warning in &output.file_warnings {
        rendered.push_str(&format!("{}\n", format_validate_warning(warning)));
    }
}

fn append_run_frames_group(rendered: &mut String, first_group: &mut bool, output: &RunOutput) {
    append_spaced_validate_heading(rendered, first_group, "frames");
    for file in &output.files {
        append_run_file_group(rendered, file);
    }
    for skipped in &output.skipped_files {
        append_run_skipped_file_group(rendered, skipped);
    }
}

fn append_run_file_group(rendered: &mut String, file: &RunFileOutput) {
    append_run_frame_subtitle(rendered, &file.label);
    append_run_file_path(rendered, &file.image);
    rendered.push_str(&format!("tags: {}\n", file.result.written));
    rendered.push_str(&format!("skipped: {}\n", file.result.skipped.len()));
    for warning in &file.result.warnings {
        rendered.push_str(&format!("{}\n", format_validate_warning(warning)));
    }
    for skipped in &file.result.skipped {
        rendered.push_str(&format!(
            "{}\n",
            format_validate_warning(&format!("skipped {skipped}"))
        ));
    }
    for error in &file.result.errors {
        rendered.push_str(&format!("{}\n", format_validate_error(error)));
    }
}

fn append_run_skipped_file_group(rendered: &mut String, image: &Path) {
    append_run_frame_subtitle(rendered, &run_file_heading(image));
    append_run_file_path(rendered, image);
    rendered.push_str("skipped: no metadata\n");
}

fn append_run_frame_subtitle(rendered: &mut String, label: &str) {
    rendered.push_str(&format!("{}\n", label.bright_cyan()));
}

fn run_file_heading(image: &Path) -> String {
    file_name(image)
}

fn append_run_file_path(rendered: &mut String, image: &Path) {
    if !is_current_directory_file(image) {
        rendered.push_str(&format!("file: {}\n", image.display()));
    }
}

fn is_current_directory_file(image: &Path) -> bool {
    image
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty() || parent == Path::new("."))
}

fn append_run_overview_group(rendered: &mut String, first_group: &mut bool, summary: &RunSummary) {
    append_spaced_validate_heading(rendered, first_group, "overview");
    rendered.push_str(&format!("errors      {}\n", summary.errors));
    rendered.push_str(&format!("warnings    {}\n", summary.warnings));
    rendered.push_str(&format!("written     {}\n", summary.written_tags));
    rendered.push_str(&format!("skipped     {}\n", summary.skipped_tags));
    rendered.push_str(&format!("files skipped {}\n", summary.skipped_files));
    rendered.push_str(&format!("took {} ms\n", summary.elapsed_ms));

    if summary.errors > 0 {
        rendered.push_str(&format!("run: {}\n", "fail".red()));
    } else {
        rendered.push_str(&format!("run: {}\n", "success".green()));
    }
}

enum RunTagError {
    Ignored,
    Warning(String),
}

fn is_blank_yaml_value(value: &YamlValue) -> bool {
    matches!(value, YamlValue::Null)
        || matches!(value, YamlValue::String(value) if value.trim().is_empty())
}

fn expand_location_tag(value: &YamlValue) -> Result<Vec<WritableExifTag>, RunTagError> {
    let Some(location_name) = location_name_from_yaml(value, &mut Vec::new()) else {
        return Err(RunTagError::Ignored);
    };
    let geonames = open_embedded_geonames_database()
        .map_err(|error| RunTagError::Warning(format!("$Location lookup failed: {error}")))?;
    let locations = locations_by_name(&geonames, location_name)
        .map_err(|error| RunTagError::Warning(format!("$Location lookup failed: {error}")))?;
    let Some(location) = locations.first() else {
        return Err(RunTagError::Warning(format!(
            "$Location: no match found in database [for <{location_name}>]"
        )));
    };

    Ok(gps_tags(location.latitude, location.longitude, None))
}

fn gps_tags(latitude: f64, longitude: f64, altitude: Option<f64>) -> Vec<WritableExifTag> {
    let mut tags = vec![
        WritableExifTag::GPSLatitudeRef(if latitude < 0.0 { "S" } else { "N" }.to_string()),
        WritableExifTag::GPSLatitude(decimal_to_dms_rational(latitude.abs())),
        WritableExifTag::GPSLongitudeRef(if longitude < 0.0 { "W" } else { "E" }.to_string()),
        WritableExifTag::GPSLongitude(decimal_to_dms_rational(longitude.abs())),
        WritableExifTag::GPSMapDatum("WGS-84".to_string()),
    ];

    if let Some(altitude) = altitude {
        tags.push(WritableExifTag::GPSAltitudeRef(vec![if altitude < 0.0 {
            1
        } else {
            0
        }]));
        tags.push(WritableExifTag::GPSAltitude(vec![rational(altitude.abs())]));
    }

    tags
}

fn decimal_to_dms_rational(decimal: f64) -> Vec<uR64> {
    let degrees = decimal.trunc();
    let minutes_float = (decimal - degrees) * 60.0;
    let minutes = minutes_float.trunc();
    let seconds = (minutes_float - minutes) * 60.0;

    vec![
        rational(degrees),
        rational(minutes),
        rational_with_denominator(seconds, 10_000),
    ]
}

fn writable_exif_tag(name: &str, value: &YamlValue) -> Option<WritableExifTag> {
    let tag = match name {
        "Artist" | "Photographer" => WritableExifTag::Artist(yaml_string(value)?),
        "Copyright" => WritableExifTag::Copyright(yaml_string(value)?),
        "CreateDate" => WritableExifTag::CreateDate(yaml_datetime(value)?),
        "DateTimeOriginal" => WritableExifTag::DateTimeOriginal(yaml_datetime(value)?),
        "ExposureProgram" => WritableExifTag::ExposureProgram(vec![yaml_u16(value)?]),
        "ExposureTime" => WritableExifTag::ExposureTime(vec![yaml_rational(value)?]),
        "FNumber" => WritableExifTag::FNumber(vec![yaml_rational(value)?]),
        "FileSource" => WritableExifTag::FileSource(vec![yaml_u8(value)?]),
        "Flash" => WritableExifTag::Flash(vec![yaml_u16(value)?]),
        "FocalLength" => WritableExifTag::FocalLength(vec![yaml_rational(value)?]),
        "GPSAltitude" => WritableExifTag::GPSAltitude(vec![yaml_rational(value)?]),
        "GPSAltitudeRef" => WritableExifTag::GPSAltitudeRef(vec![yaml_u8(value)?]),
        "GPSLatitude" => {
            WritableExifTag::GPSLatitude(decimal_to_dms_rational(yaml_f64(value)?.abs()))
        }
        "GPSLatitudeRef" => WritableExifTag::GPSLatitudeRef(yaml_string(value)?),
        "GPSLongitude" => {
            WritableExifTag::GPSLongitude(decimal_to_dms_rational(yaml_f64(value)?.abs()))
        }
        "GPSLongitudeRef" => WritableExifTag::GPSLongitudeRef(yaml_string(value)?),
        "GPSMapDatum" => WritableExifTag::GPSMapDatum(yaml_string(value)?),
        "ISO" | "ISOSpeedRatings" => WritableExifTag::ISO(vec![yaml_u16(value)?]),
        "ISOSpeed" => WritableExifTag::ISOSpeed(vec![yaml_u32(value)?]),
        "ImageDescription" => WritableExifTag::ImageDescription(yaml_string(value)?),
        "LensMake" => WritableExifTag::LensMake(yaml_string(value)?),
        "LensModel" => WritableExifTag::LensModel(yaml_string(value)?),
        "LightSource" => WritableExifTag::LightSource(vec![yaml_u16(value)?]),
        "Make" => WritableExifTag::Make(yaml_string(value)?),
        "MaxApertureValue" => WritableExifTag::MaxApertureValue(vec![yaml_rational(value)?]),
        "MeteringMode" => WritableExifTag::MeteringMode(vec![yaml_u16(value)?]),
        "Model" => WritableExifTag::Model(yaml_string(value)?),
        "ModifyDate" => WritableExifTag::ModifyDate(yaml_datetime(value)?),
        "Orientation" => WritableExifTag::Orientation(vec![yaml_u16(value)?]),
        "Software" => WritableExifTag::Software(yaml_string(value)?),
        "WhiteBalance" => WritableExifTag::WhiteBalance(vec![yaml_u16(value)?]),
        _ => return None,
    };

    Some(tag)
}

fn writable_tag_name(tag: &WritableExifTag) -> Option<&'static str> {
    match tag {
        WritableExifTag::Artist(_) => Some("Artist"),
        WritableExifTag::Copyright(_) => Some("Copyright"),
        WritableExifTag::CreateDate(_) => Some("CreateDate"),
        WritableExifTag::DateTimeOriginal(_) => Some("DateTimeOriginal"),
        WritableExifTag::ExposureProgram(_) => Some("ExposureProgram"),
        WritableExifTag::ExposureTime(_) => Some("ExposureTime"),
        WritableExifTag::FNumber(_) => Some("FNumber"),
        WritableExifTag::FileSource(_) => Some("FileSource"),
        WritableExifTag::Flash(_) => Some("Flash"),
        WritableExifTag::FocalLength(_) => Some("FocalLength"),
        WritableExifTag::GPSAltitude(_) => Some("GPSAltitude"),
        WritableExifTag::GPSAltitudeRef(_) => Some("GPSAltitudeRef"),
        WritableExifTag::GPSLatitude(_) => Some("GPSLatitude"),
        WritableExifTag::GPSLatitudeRef(_) => Some("GPSLatitudeRef"),
        WritableExifTag::GPSLongitude(_) => Some("GPSLongitude"),
        WritableExifTag::GPSLongitudeRef(_) => Some("GPSLongitudeRef"),
        WritableExifTag::GPSMapDatum(_) => Some("GPSMapDatum"),
        WritableExifTag::ISO(_) => Some("ISO"),
        WritableExifTag::ISOSpeed(_) => Some("ISOSpeed"),
        WritableExifTag::ImageDescription(_) => Some("ImageDescription"),
        WritableExifTag::LensMake(_) => Some("LensMake"),
        WritableExifTag::LensModel(_) => Some("LensModel"),
        WritableExifTag::LightSource(_) => Some("LightSource"),
        WritableExifTag::Make(_) => Some("Make"),
        WritableExifTag::MaxApertureValue(_) => Some("MaxApertureValue"),
        WritableExifTag::MeteringMode(_) => Some("MeteringMode"),
        WritableExifTag::Model(_) => Some("Model"),
        WritableExifTag::ModifyDate(_) => Some("ModifyDate"),
        WritableExifTag::Orientation(_) => Some("Orientation"),
        WritableExifTag::Software(_) => Some("Software"),
        WritableExifTag::WhiteBalance(_) => Some("WhiteBalance"),
        _ => None,
    }
}

fn yaml_string(value: &YamlValue) -> Option<String> {
    match value {
        YamlValue::Null => None,
        YamlValue::String(value) => {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        }
        YamlValue::Number(value) => Some(value.to_string()),
        YamlValue::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn yaml_datetime(value: &YamlValue) -> Option<String> {
    let value = yaml_string(value)?;
    if value.len() == 10 && value.as_bytes().get(4) == Some(&b'-') {
        Some(format!(
            "{}:{}:{} 00:00:00",
            &value[0..4],
            &value[5..7],
            &value[8..10]
        ))
    } else {
        Some(value.replace('-', ":"))
    }
}

fn yaml_u8(value: &YamlValue) -> Option<u8> {
    u8::try_from(yaml_u32(value)?).ok()
}

fn yaml_u16(value: &YamlValue) -> Option<u16> {
    u16::try_from(yaml_u32(value)?).ok()
}

fn yaml_u32(value: &YamlValue) -> Option<u32> {
    match value {
        YamlValue::Number(number) => number.as_u64().and_then(|value| u32::try_from(value).ok()),
        YamlValue::String(value) => clean_numeric_string(value).parse::<u32>().ok(),
        YamlValue::Bool(value) => Some(u32::from(*value)),
        _ => None,
    }
}

fn yaml_f64(value: &YamlValue) -> Option<f64> {
    match value {
        YamlValue::Number(number) => number.as_f64(),
        YamlValue::String(value) => parse_number_or_fraction(value),
        _ => None,
    }
}

fn yaml_rational(value: &YamlValue) -> Option<uR64> {
    if let YamlValue::String(value) = value {
        let value = clean_numeric_string(value);
        if let Some((numerator, denominator)) = value.split_once('/') {
            let numerator = clean_numeric_string(numerator).parse::<u32>().ok()?;
            let denominator = clean_numeric_string(denominator).parse::<u32>().ok()?;
            return (denominator != 0).then_some(uR64 {
                nominator: numerator,
                denominator,
            });
        }
    }

    yaml_f64(value).map(rational)
}

fn clean_numeric_string(value: &str) -> String {
    let value = value.trim();
    let value = value
        .strip_prefix("f/")
        .or_else(|| value.strip_prefix("F/"))
        .unwrap_or(value);

    value
        .trim_end_matches("mm")
        .trim_end_matches("MM")
        .trim()
        .to_string()
}

fn parse_number_or_fraction(value: &str) -> Option<f64> {
    let value = clean_numeric_string(value);
    if let Some((numerator, denominator)) = value.split_once('/') {
        let numerator = numerator.trim().parse::<f64>().ok()?;
        let denominator = denominator.trim().parse::<f64>().ok()?;
        return (denominator != 0.0).then_some(numerator / denominator);
    }
    value.parse::<f64>().ok()
}

fn rational(value: f64) -> uR64 {
    rational_with_denominator(value, 1_000_000)
}

fn rational_with_denominator(value: f64, denominator: u32) -> uR64 {
    let value = value.max(0.0);
    uR64 {
        nominator: (value * f64::from(denominator)).round() as u32,
        denominator,
    }
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
    fn run_request_treats_single_non_metadata_argument_as_targets() {
        let args = RunArgs {
            metadata_or_targets: Some(PathBuf::from("*.jpg")),
            targets: None,
            strip: false,
            no_overwrite: false,
            extensions: Vec::new(),
            recursive: false,
        };

        let request = RunRequest::from_args(&args);

        assert_eq!(request.metadata, None);
        assert_eq!(request.targets, Some("*.jpg".to_string()));
    }

    #[test]
    fn run_targets_filter_default_images_by_extension() {
        let directory = temporary_test_directory("run-target-extensions");
        std::fs::write(directory.join("a.jpg"), [0xff, 0xd8, 0xff, 0xd9])
            .expect("jpg should be written");
        std::fs::write(directory.join("b.tif"), [0x49, 0x49, 0x2a, 0x00])
            .expect("tif should be written");

        let targets = resolve_run_targets(&directory, None, false, &["jpg".to_string()])
            .expect("targets should resolve");

        assert_eq!(targets, [directory.join("a.jpg")]);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn run_plan_merges_global_and_frame_tags() {
        let directory = temporary_test_directory("run-plan-merge");
        let metadata = directory.join(METADATA_FILE_NAME);
        let image = directory.join("one.jpg");
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
exif:
  Make: Nikon
  Model: F3
frames:
  1:
    - Model: FM2
    - ExposureTime: 1/500
"#,
        )
        .expect("test YAML should parse");

        let plan = build_run_plan(&metadata, &yaml, std::slice::from_ref(&image))
            .expect("run plan should build");
        let frame = plan
            .frame_for_image(&image)
            .expect("image should have merged tags");
        let tags = frame.tags;

        assert_eq!(tags.len(), 3);
        assert_eq!(frame.label, "frame 1 (one.jpg)");
        assert!(
            tags.iter()
                .any(|tag| tag.name == "Make" && tag.value.as_str() == Some("Nikon"))
        );
        assert!(
            tags.iter()
                .any(|tag| tag.name == "Model" && tag.value.as_str() == Some("FM2"))
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn run_plan_expands_iso_aliases_after_frame_overrides() {
        let directory = temporary_test_directory("run-plan-iso");
        let metadata = directory.join(METADATA_FILE_NAME);
        let image = directory.join("one.jpg");
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
exif:
  ISO: 400
frames:
  1:
    - ISOSpeed: 800
"#,
        )
        .expect("test YAML should parse");

        let plan = build_run_plan(&metadata, &yaml, std::slice::from_ref(&image))
            .expect("run plan should build");
        let tags = plan
            .frame_for_image(&image)
            .expect("image should have merged tags")
            .tags;

        for name in ["ISO", "ISOSpeed", "ISOSpeedRatings"] {
            assert!(
                tags.iter()
                    .any(|tag| tag.name == name && tag.value.as_i64() == Some(800)),
                "{name} should be expanded with the frame override"
            );
        }

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn run_plan_labels_filename_frame_keys_with_file_name() {
        let directory = temporary_test_directory("run-plan-filename-label");
        let metadata = directory.join(METADATA_FILE_NAME);
        let image = directory.join("2.jpg");
        let yaml = serde_yaml::from_str::<YamlValue>(
            r#"
frames:
  "2.jpg":
    Make: Nikon
"#,
        )
        .expect("test YAML should parse");

        let plan = build_run_plan(&metadata, &yaml, std::slice::from_ref(&image))
            .expect("run plan should build");
        let frame = plan
            .frame_for_image(&image)
            .expect("filename frame should match image");

        assert_eq!(frame.label, "2.jpg");

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn run_output_renders_file_groups_and_overview() {
        colored::control::set_override(true);
        let image = PathBuf::from("image.jpg");
        let output = RunOutput {
            metadata_path: PathBuf::from("metadata.yaml"),
            dry_run: true,
            target_count: 3,
            file_warnings: Vec::new(),
            files: vec![
                RunFileOutput {
                    label: "frame 1 (image.jpg)".to_string(),
                    image: image.clone(),
                    result: RunFileResult {
                        written: 2,
                        warnings: vec!["skipping unsupported writer tag `FilmRoll`".to_string()],
                        ..RunFileResult::default()
                    },
                },
                RunFileOutput {
                    label: "2.jpg".to_string(),
                    image: PathBuf::from("2.jpg"),
                    result: RunFileResult {
                        written: 1,
                        ..RunFileResult::default()
                    },
                },
            ],
            skipped_files: vec![PathBuf::from("missing.jpg")],
        };
        let mut summary = RunSummary::default();
        summary.add(&output.files[0].result);
        summary.add(&output.files[1].result);
        summary.skipped_files = output.skipped_files.len();
        summary.elapsed_ms = 42;

        let rendered = format_run_output(&output, &summary);
        let plain = strip_ansi_codes(&rendered);

        assert!(plain.starts_with("run "));
        assert!(plain.contains("mode: dry-run\n\nframes "));
        assert!(plain.contains("frames "));
        assert!(plain.contains("frame 1 (image.jpg)\ntags: 2"));
        assert!(rendered.contains("\u{1b}[94mframes"));
        assert!(rendered.contains("\u{1b}[96mframe 1 (image.jpg)"));
        assert!(rendered.contains("\u{1b}[96m2.jpg"));
        assert!(!plain.contains("file: image.jpg"));
        assert!(plain.contains("warning: skipping unsupported writer tag `FilmRoll`"));
        assert!(
            plain.contains("warning: skipping unsupported writer tag `FilmRoll`\n2.jpg\ntags: 1")
        );
        assert!(plain.contains("skipped: 0\nmissing.jpg\nskipped: no metadata"));
        assert!(plain.contains("skipped: no metadata\n\noverview "));
        assert!(rendered.contains("\u{1b}[94moverview"));
        assert!(plain.contains("errors      0"));
        assert!(plain.contains("warnings    1"));
        assert!(plain.contains("took 42 ms\nrun: success"));
        assert!(rendered.contains("run: \u{1b}[32msuccess"));
    }

    #[test]
    fn run_output_prints_path_for_non_current_directory_files() {
        colored::control::set_override(true);
        let output = RunOutput {
            metadata_path: PathBuf::from("metadata.yaml"),
            dry_run: true,
            target_count: 1,
            file_warnings: Vec::new(),
            files: vec![RunFileOutput {
                label: "image.jpg".to_string(),
                image: PathBuf::from("nested").join("image.jpg"),
                result: RunFileResult {
                    written: 1,
                    ..RunFileResult::default()
                },
            }],
            skipped_files: Vec::new(),
        };
        let mut summary = RunSummary::default();
        summary.add(&output.files[0].result);

        let rendered = format_run_output(&output, &summary);
        let plain = strip_ansi_codes(&rendered);

        assert!(rendered.contains("\u{1b}[96mimage.jpg"));
        assert!(
            plain.contains("file: nested\\image.jpg") || plain.contains("file: nested/image.jpg")
        );
    }

    #[test]
    fn custom_tag_payload_round_trips_yaml_values() {
        let tags = vec![
            CustomTag {
                name: "FilmRoll".to_string(),
                value: YamlValue::Number(35.into()),
            },
            CustomTag {
                name: "FilmName".to_string(),
                value: YamlValue::String("Kodak Double-X".to_string()),
            },
            CustomTag {
                name: "FilmNegative".to_string(),
                value: YamlValue::Bool(true),
            },
        ];

        let payload = encode_custom_tags(&tags).expect("custom tags should encode");
        let decoded = custom_tags_from_bytes(&payload).expect("custom tags should decode");
        let json = std::str::from_utf8(
            payload
                .strip_prefix(USER_COMMENT_ASCII_PREFIX)
                .expect("custom tags should use the EXIF ASCII UserComment prefix"),
        )
        .expect("custom tag JSON should be UTF-8");

        assert_eq!(decoded, tags);
        assert!(!payload.starts_with(LEGACY_CUSTOM_TAG_PAYLOAD_PREFIX));
        assert!(json.starts_with(CUSTOM_TAG_PAYLOAD_MARKER));
        assert!(json.contains("exifmeta-v0.1.0"));
        assert!(json.contains(r#""FilmRoll":35"#));
        assert!(json.contains(r#""FilmName":"Kodak Double-X""#));
        assert!(json.contains(r#""FilmNegative":true"#));
    }

    #[test]
    fn custom_tag_payload_decodes_marker_bare_json_and_legacy_yaml() {
        let bare_json = br#"{"FilmRoll":35,"FilmName":"Kodak Double-X"}"#;
        let mut marked_json = CUSTOM_TAG_PAYLOAD_MARKER.as_bytes().to_vec();
        marked_json.extend_from_slice(bare_json);
        let mut ascii_prefixed_json = USER_COMMENT_ASCII_PREFIX.to_vec();
        ascii_prefixed_json.extend_from_slice(bare_json);
        let mut legacy = LEGACY_CUSTOM_TAG_PAYLOAD_PREFIX.to_vec();
        legacy.extend_from_slice(b"FilmRoll: 35\nFilmName: Kodak Double-X\n");

        let bare_decoded = custom_tags_from_bytes(bare_json).expect("bare JSON should decode");
        let marked_decoded =
            custom_tags_from_bytes(&marked_json).expect("marked JSON should decode");
        let ascii_prefixed_decoded = custom_tags_from_bytes(&ascii_prefixed_json)
            .expect("ASCII-prefixed JSON should decode");
        let legacy_decoded =
            custom_tags_from_bytes(&legacy).expect("legacy YAML payload should decode");

        assert_eq!(
            bare_decoded,
            vec![
                CustomTag {
                    name: "FilmRoll".to_string(),
                    value: YamlValue::Number(35.into()),
                },
                CustomTag {
                    name: "FilmName".to_string(),
                    value: YamlValue::String("Kodak Double-X".to_string()),
                },
            ]
        );
        assert_eq!(marked_decoded, bare_decoded);
        assert_eq!(ascii_prefixed_decoded, bare_decoded);
        assert_eq!(legacy_decoded, bare_decoded);
    }

    #[test]
    fn run_dry_run_counts_custom_tags_without_warning() {
        let args = RunArgs {
            metadata_or_targets: None,
            targets: None,
            strip: false,
            no_overwrite: false,
            extensions: Vec::new(),
            recursive: false,
        };
        let tags = vec![
            RunTag {
                name: "Make".to_string(),
                value: YamlValue::String("Nikon".to_string()),
            },
            RunTag {
                name: "FilmRoll".to_string(),
                value: YamlValue::Number(35.into()),
            },
        ];

        let result = apply_tags_to_image(Path::new("image.jpg"), &tags, true, &args);

        assert_eq!(result.written, 2);
        assert!(result.warnings.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn inspect_decodes_custom_tags_from_user_comment() {
        colored::control::set_override(true);
        let payload = encode_custom_tags(&[
            CustomTag {
                name: "FilmRoll".to_string(),
                value: YamlValue::Number(35.into()),
            },
            CustomTag {
                name: "FilmName".to_string(),
                value: YamlValue::String("Kodak Double-X".to_string()),
            },
        ])
        .expect("custom tags should encode");
        let entry = tiff_undefined_entry(0x9286, payload.len(), 200);
        let metadata = InspectMetadata {
            exif: parse_raw_exif_with_exif_entries(&[entry], &[(200, payload)]),
            warnings: Vec::new(),
            file_info: InspectFileInfo {
                rows: vec![InspectInfoRow::new("Image Width", "100 px")],
            },
        };

        let pretty = strip_ansi_codes(&format_inspect_output(
            Path::new("image.jpg"),
            &metadata,
            InspectFormat::Pretty,
        ));
        let raw = strip_ansi_codes(&format_inspect_output(
            Path::new("image.jpg"),
            &metadata,
            InspectFormat::Raw,
        ));

        assert!(pretty.contains("custom "));
        assert!(pretty.contains("Film Roll  35"));
        assert!(pretty.contains("Film Name  Kodak Double-X"));
        assert!(pretty.contains("misc "));
        assert!(pretty.contains("Image Width  100 px"));
        assert!(pretty.find("custom ").unwrap() < pretty.find("misc ").unwrap());
        assert!(!pretty.contains("User Comment"));
        assert!(raw.contains("0x9286"));
        assert!(raw.contains("UserComment"));
        assert!(raw.contains("exifmeta-v0.1.0"));
        assert!(raw.contains("FilmRoll"));
        assert!(raw.contains("Kodak Double-X"));
        assert!(!raw.contains("IFD exifmeta  Custom  0x0000"));
    }

    #[test]
    fn run_tag_parser_supports_fraction_and_unit_values() {
        let exposure = writable_exif_tag("ExposureTime", &YamlValue::String("1/500".to_string()))
            .expect("exposure tag should parse");
        let focal_length = writable_exif_tag("FocalLength", &YamlValue::String("75mm".to_string()))
            .expect("focal length tag should parse");
        let aperture = writable_exif_tag("FNumber", &YamlValue::String("f/5.6".to_string()))
            .expect("aperture tag should parse");

        assert!(
            matches!(exposure, WritableExifTag::ExposureTime(values) if values[0].nominator == 1 && values[0].denominator == 500)
        );
        assert!(
            matches!(focal_length, WritableExifTag::FocalLength(values) if values[0].nominator == 75_000_000 && values[0].denominator == 1_000_000)
        );
        assert!(
            matches!(aperture, WritableExifTag::FNumber(values) if values[0].nominator == 5_600_000 && values[0].denominator == 1_000_000)
        );
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
        assert!(rendered.starts_with("file "));
        assert!(rendered.contains("YAML format: ok\n\nexif "));
        assert!(rendered.contains("standard tags: 1\nunknown tags: 0\n\nframes "));
        assert!(rendered.contains("unknown tags: 0\n\noverview "));
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

        assert!(plain_output.contains("file "));
        assert!(plain_output.contains("File Name  image.tif"));
        assert!(plain_output.contains("camera "));
        assert!(plain_output.contains("Make   \"Z\"\nModel  \"E\""));
        assert!(plain_output.contains("unknown "));
        assert!(plain_output.contains("Unknown Tiff Tag  Short([42])"));
        assert!(output.contains("Warnings:"));
        assert!(output.contains("warning: ignored malformed trailing field"));
        assert!(plain_output.find("file ").unwrap() < plain_output.find("camera ").unwrap());
        assert!(plain_output.find("camera ").unwrap() < plain_output.find("unknown ").unwrap());
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

        assert!(plain_output.starts_with("camera "));
        assert!(!plain_output.contains("file "));
        assert!(!plain_output.contains("film "));
        assert!(!plain_output.contains("exposure "));
        assert!(!plain_output.contains("gps "));
        assert!(!plain_output.contains("misc "));
        assert!(!plain_output.contains("unknown "));
    }

    #[test]
    fn pretty_inspect_group_heading_uses_validate_style_blue_rule() {
        colored::control::set_override(true);
        let metadata = InspectMetadata {
            exif: parse_raw_exif(&[tiff_ascii_entry(0x010f, b"Z\0")]),
            warnings: Vec::new(),
            file_info: InspectFileInfo::empty(),
        };

        let output =
            format_inspect_output(Path::new("image.tif"), &metadata, InspectFormat::Pretty);
        colored::control::set_override(false);
        let expected = format!("\u{1b}[94mcamera {}\u{1b}[0m", "─".repeat(43));

        assert_eq!(output.lines().next(), Some(expected.as_str()));
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

        assert!(plain_output.contains("gps "));
        assert_eq!(plain_output.matches("GPS Nearest Location").count(), 5);
        assert_eq!(plain_output.matches("GPS Nearest City").count(), 1);
        assert_eq!(plain_output.matches("GPS Nearest Large City").count(), 0);
        assert!(plain_output.contains("GPS Nearest Location 1  (1.9 km) Dunchurch, GB"));
        assert!(plain_output.contains("GPS Nearest Location 2  (3.2 km) Long Lawford, GB"));
        assert!(plain_output.contains("GPS Nearest City        (15.3 km) Coventry, GB"));
        assert!(
            plain_output
                .find("GPS Nearest Location 5")
                .expect("nearest location rows should be present")
                < plain_output
                    .find("GPS Nearest City")
                    .expect("nearest city rows should be present")
        );
        assert!(output.contains("\u{1b}[32mGPS Nearest Location 1\u{1b}[0m"));
        assert!(output.contains("\u{1b}[32mGPS Nearest City\u{1b}[0m"));
        assert!(!output.contains("\u{1b}[32mGPS Nearest Large City\u{1b}[0m"));
    }

    #[test]
    fn pretty_inspect_output_omits_nearest_city_when_it_duplicates_nearest_location() {
        let metadata = test_coventry_gps_metadata();

        let output =
            format_inspect_output(Path::new("image.jpg"), &metadata, InspectFormat::Pretty);
        let plain_output = strip_ansi_codes(&output);

        assert_eq!(plain_output.matches("GPS Nearest Location").count(), 5);
        assert_eq!(plain_output.matches("GPS Nearest City").count(), 0);
        assert_eq!(plain_output.matches("Coventry, GB").count(), 1);
        assert!(plain_output.contains("GPS Nearest Location 1  (0 m) Coventry, GB"));
    }

    #[test]
    fn raw_inspect_output_omits_nearest_locations() {
        let metadata = test_gps_metadata();

        let output = format_inspect_output(Path::new("image.jpg"), &metadata, InspectFormat::Raw);

        assert!(output.contains("GPSLatitude"));
        assert!(output.contains("GPSLatitudeRef"));
        assert!(output.contains("GPSLongitudeRef"));
        assert!(!output.contains("GPS Nearest Location"));
        assert!(!output.contains("GPS Nearest City"));
        assert!(!output.contains("GPS Nearest Large City"));
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
        assert!(!output.contains("GPS Nearest Location"));
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

    fn test_coventry_gps_metadata() -> InspectMetadata {
        let (latitude_entry, latitude_data) =
            tiff_rational_entry(0x0002, [(52, 1), (24, 1), (23616, 1000)], 200);
        let (longitude_entry, longitude_data) =
            tiff_rational_entry(0x0004, [(1, 1), (30, 1), (43812, 1000)], 224);

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

    fn tiff_undefined_entry(tag: u16, length: usize, offset: u32) -> [u8; 12] {
        let mut entry = [0; 12];
        entry[0..2].copy_from_slice(&tag.to_be_bytes());
        entry[2..4].copy_from_slice(&7u16.to_be_bytes());
        entry[4..8].copy_from_slice(&(length as u32).to_be_bytes());
        entry[8..12].copy_from_slice(&offset.to_be_bytes());
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
