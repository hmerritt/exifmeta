# Exif Metadata

EXIF tool for photographers.

A simple program to read a standardised `metadata.yaml` file and write the data as EXIF to all image files in the same directory.

## Features ⚡

- EXIF viewer
- Custom EXIF properties are supported
- Automatically bulk add EXIF to images in the current directory

## CLI Commands

| Command       | Function                                                                             |
| :------------ | :----------------------------------------------------------------------------------- |
| `run`         | Main function; reads `metadata.yaml` file and writes EXIF data to target image files |
| `init`        | Create template `metadata.yaml` file                                                 |
| `validate`    | Checks `metadata.yaml` is valid                                                      |
| `inspect`     | Read and pretty-print the current EXIF data of a specific image file                 |
| `interactive` | Interactively read and set EXIF data for any image                                   |
| `strip`       | Removes all existing EXIF metadata from target image files                           |

### Flags

| Command          | Function                                                                                                                                   |
| :--------------- | :----------------------------------------------------------------------------------------------------------------------------------------- |
| `--dry-run`      | Runs the program in 'simulation' mode, without making any changes to any files                                                             |
| `--strip`        | With `run`, remove all existing EXIF data from each file before adding new data                                                            |
| `--no-overwrite` | Prevents overwriting exif data if there is already data there                                                                              |
| `--extensions`   | Restricts processing to specified file typologies to prevent the script from attempting to modify unsupported binaries (e.g., -e jpg,tiff) |
| `--recursive`    | Find image files across all subdirectories, applying the root configuration to nested image repositories                                   |
| `--verify`       | Re-read images after `strip` and fail if EXIF metadata remains                                                                             |
| `--keep`         | With `run` or `strip`, remove all EXIF tags except the comma-separated tag names                                                           |
| `--remove`       | With `run` or `strip`, remove the comma-separated tag names; can combine with `--keep` or `--privacy` and takes precedence                 |
| `--privacy`      | With `run` or `strip`, remove known privacy-sensitive EXIF tags while keeping harmless technical and unknown tags                          |
| `--json`         | Emit machine-readable JSON output for `strip`                                                                                              |

### Supported Image File Formats

- JPEG / JPG
- JXL
- HEIF / HEIC / HIF / AVIF
- PNG
- TIFF
- WebP (only lossless and extended)

## `metadata.yaml` file

- [EXIF Tags reference](https://exiftool.org/TagNames/EXIF.html)
- [Locations reference](https://www.geonames.org)

See example files in [`./examples`](./examples/metadata.yml) directory. `examples/metadata.yml`:

```yaml
# ───────────────────────────────────────────────
# Custom Properties
# These values will not be written as EXIF, and are meant for personal organisational purposes — e.g. private metadata for your shoot
# ───────────────────────────────────────────────
roll: 35
date: 2026-04-28
date_end: 2026-04-29
frame_count: 15
notable_frames: [5, 9, 15]
locations: [Wales]

# ───────────────────────────────────────────────
# Global EXIF Properties
# Any valid EXIF tag can be set here. These tags will be written to ALL images.
# ───────────────────────────────────────────────
exif:
    # Camera & Lens
    Make: Zenza Bronica
    Model: ETRS
    LensMake: Zenza Bronica
    LensModel: Zenzanon 75mm f/2.8
    FocalLength: 75mm
    MaxApertureValue: 2.8

    # Film / Capture
    ISOSpeedRatings: 250 # ISOSpeedRatings | exif:ISO | exifEX:ISOSpeed
    DateTimeOriginal: "2026-04-28"
    CreateDate: "2026-04-28"
    # 1 = Film Scanner
    # 2 = Reflection Print Scanner
    # 3 = Digital Camera
    FileSource: 1

    # AnalogueData
    # Film
    FilmRoll: 35
    FilmMaker: CineStill Kodak
    FilmName: Kodak Double-X
    FilmFormat: 120
    FilmColor: false
    FilmNegative: true
    # Film Development
    FilmDevelopProcess: B&W
    FilmDeveloper:
    FilmProcessLab: The Darkroom, UK
    FilmProcessDate: 2026-04-30
    FilmScanner: Noritsu

    # Attribution
    Artist: Harry Merritt
    Photographer: Harry Merritt

# ───────────────────────────────────────────────
# Per Frame/File EXIF Properties
# Use this to set EXIF tags for individual files, like ExposureTime, FNumber, or GPS data.
# Values set here will override the above `exif` values.
# ───────────────────────────────────────────────
frames:
    # Frame number (first file when sorted alphabetically, useful when shooting film and files are in-order)
    1:
        - ImageDescription:
        - ExposureTime: 1/500
        - FNumber: 2.8
        # Special key (`$` prefix) that will match city/town names to GPS long/lat values automatically,
        # uses an embeded database, no internet requried.
        - $Location: Betws-y-Coed

    # Filename (direct but more verbose)
    "image-file.tif":
        - ExposureTime: 1/250
        - FNumber: 5.6
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
        - GPSLatitude: "51.5074"
        - GPSLatitudeRef: "N"
        - GPSLongitude: "3.1791"
        - GPSLongitudeRef: "W"
        - GPSAltitude: "142"
        - GPSAltitudeRef: 0 # 0 = above sea level
        - GPSMapDatum: "WGS-84"
```

---

## Development

Development and testing can be Windows, Linux, and macOS.

### Prerequisites

- Rust stable toolchain (rustup, cargo)
- Windows development: MSVC toolchain/Visual Studio Build Tools (C++ build tools)
- Linux/macOS: standard native build tools (clang/gcc and linker)

```sh
rustup toolchain install stable
```

### GeoNames database generation

`tools/geonames_to_sqlite.py` converts the GeoNames `cities1000.txt` dump into the compact SQLite database used for location lookups.

Download `cities1000.zip` from the [GeoNames export dump](https://download.geonames.org/export/dump/), extract `cities1000.txt`, and place it in the repository root. The script only uses the Python standard library.

```sh
python tools/geonames_to_sqlite.py
```

By default, the script reads `cities1000.txt` and writes `assets/geonames/cities1000.sqlite`. To use different paths:

```sh
python tools/geonames_to_sqlite.py --input path/to/cities1000.txt --output assets/geonames/cities1000.sqlite
```

Each run deletes and recreates the output database. The generated database contains a `locations` table with `geoname_id`, `name`, `country_code`, `latitude`, `longitude`, `population`, and `elevation`, plus an index on case-insensitive `name` and `country_code`.

During import, the script validates that each GeoNames row has 19 tab-separated fields and parses the ID, coordinates, population, and any non-empty elevation as numeric values. When it finishes, it prints the number of rows written and the final SQLite file size.

### Implementation

- [https://github.com/TechnikTobi/little_exif](TechnikTobi/little_exif) — A library for reading and writing EXIF data in pure Rust.
- [GeoNames](https://www.geonames.org/) — The GeoNames geographical database `cities1000.zip`. Used to match location names to EXIF GPS data.

#### Features (maybe)

```

```
