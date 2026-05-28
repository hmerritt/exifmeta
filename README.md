<img src="./assets/icon.png" draggable="false" width="100px" />

# Exif Metadata

[![Release](https://img.shields.io/github/v/release/hmerritt/exifmeta?link=https%3A%2F%2Fgithub.com%2Fhmerritt%2Fexifmeta%2Freleases%2Flatest)](https://github.com/hmerritt/exifmeta/releases/latest) [![Downloads](https://img.shields.io/github/downloads/hmerritt/exifmeta/total?link=https%3A%2F%2Fgithub.com%2Fhmerritt%2Fexifmeta%2Freleases%2Flatest)](https://github.com/hmerritt/exifmeta/releases/latest) [![Coverage](https://img.shields.io/coverallsCoverage/github/hmerritt/exifmeta)](https://coveralls.io/github/hmerritt/exifmeta?branch=master)

EXIF tool for photographers.

A simple program to read a standardised `metadata.yml` file and write the data as EXIF to all image files in the same directory.

## Features ⚡

- EXIF viewer
- Custom EXIF properties are supported
- Automatically bulk add EXIF to images in the current directory

## CLI Commands

| Command       | Function                                                               |
| :------------ | :--------------------------------------------------------------------- |
| `new`         | Create `metadata.yml` file                                             |
| `check`       | Checks `metadata.yml` file is valid                                    |
| `read`        | Read an image file's EXIF tags                                         |
| `write`       | Writes EXIF tags defined in `metadata.yml` to target image files       |
| `strip`       | Tool to remove all (or a select few) EXIF tags from target image files |
| `interactive` | Interactively browse folders and read image EXIF tags                  |

### Flags

| Command          | Function                                                                                                                                   |
| :--------------- | :----------------------------------------------------------------------------------------------------------------------------------------- |
| `--dry-run`      | Runs the program in 'simulation' mode, without making any changes to any files                                                             |
| `--strip`        | With `write`, remove all existing EXIF data from each file before adding new data                                                          |
| `--no-overwrite` | Prevents overwriting exif data if there is already data there                                                                              |
| `--extensions`   | Restricts processing to specified file typologies to prevent the script from attempting to modify unsupported binaries (e.g., -e jpg,tiff) |
| `--recursive`    | Find image files across all subdirectories, applying the root configuration to nested image repositories                                   |
| `--verify`       | Re-read images after `strip` and fail if EXIF metadata remains                                                                             |
| `--keep`         | With `write` or `strip`, remove all EXIF tags except the comma-separated tag names                                                         |
| `--remove`       | With `write` or `strip`, remove the comma-separated tag names; can combine with `--keep` or `--privacy` and takes precedence               |
| `--privacy`      | With `write` or `strip`, remove known privacy-sensitive EXIF tags while keeping harmless technical and unknown tags                        |
| `--json`         | Emit machine-readable JSON output for `strip`                                                                                              |

### Supported Image File Formats

- JPEG / JPG
- JXL
- HEIF / HEIC / HIF / AVIF
- PNG
- TIFF
- WebP (only lossless and extended)

## `metadata.yml` file

- [EXIF Tags reference](https://exiftool.org/TagNames/EXIF.html)
- [Locations reference](https://www.geonames.org)

See example files in [`./examples`](./examples/metadata.yml) directory. `examples/metadata.yml`:

---

<small>
    <a href="https://www.flaticon.com/free-icons/ui" title="ui icons">Ui icons created by smashingstocks - Flaticon</a>
</small>
