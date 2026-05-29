<img src="./assets/icon.png" draggable="false" width="100px" />

# `exifmeta`

[![Release](https://img.shields.io/github/v/release/hmerritt/exifmeta?link=https%3A%2F%2Fgithub.com%2Fhmerritt%2Fexifmeta%2Freleases%2Flatest)](https://github.com/hmerritt/exifmeta/releases/latest) [![Downloads](https://img.shields.io/github/downloads/hmerritt/exifmeta/total?link=https%3A%2F%2Fgithub.com%2Fhmerritt%2Fexifmeta%2Freleases%2Flatest)](https://github.com/hmerritt/exifmeta/releases/latest) [![Coverage](https://img.shields.io/coverallsCoverage/github/hmerritt/exifmeta)](https://coveralls.io/github/hmerritt/exifmeta?branch=master)

EXIF read/write/remove tool — useful for film photographers.

- [Features](#features-)
- [Download](#download-)
- [CLI Commands](#cli-commands)
- [Usage](#usage)

## Features ⚡

- Read EXIF
- Write EXIF
- Remove/Strip EXIF
- Read/Write **custom** EXIF tags
- Easily write to all images in the current directory (using a `metadata.yml` template file)
- Includes an embeded database of locations that can search GPS data to find the nearest town/city; this happens **instantly**, completely offline, without any internet connection required!

## Download 💾

#### [➡️ Manually Download The Latest Release Here](https://github.com/hmerritt/exifmeta/releases/latest)

Or via one of the supported package managers:

#### ➡️ macOS / Linux via [Homebrew](https://brew.sh/)

```sh
brew install hmerritt/tap/exifmeta
```

#### ➡️ Windows via [Scoop](https://scoop.sh/)

```sh
scoop bucket add hmerritt https://github.com/hmerritt/scoop-bucket
```

```sh
scoop install exifmeta
```

## CLI Commands

| Command                       | Function                                                               |
| :---------------------------- | :--------------------------------------------------------------------- |
| [`new`](#new)                 | Create `metadata.yml` file                                             |
| [`check`](#check)             | Checks `metadata.yml` file is valid                                    |
| [`read`](#read)               | Read an image file's EXIF tags                                         |
| [`write`](#write)             | Writes EXIF tags defined in `metadata.yml` to target image files       |
| [`strip`](#strip)             | Tool to remove all (or a select few) EXIF tags from target image files |
| [`interactive`](#interactive) | Interactively browse folders and read image EXIF tags                  |

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

### Supported Image File Formats

- JPEG / JPG
- JXL
- HEIF / HEIC / HIF / AVIF
- PNG
- TIFF
- WebP (only lossless and extended)

## Usage

### `new`

Create a `metadata.yml` file in the current directory. This creates a template file you can edit and add your own EXIF tags and organisational data to.

```sh
exifmeta new
```

Optionally, specify a directory

```sh
exifmeta new "Photos/shoot-001"
```

### `check`

Checks `metadata.yml` file is valid. If anything is wrong, the exact error will be shown and how to fix it.

Common reasons for the check to fail:

- The `metadata.yml` file does not exist
- The YAML fails to parse (the file is invalid)
- There are formatting errors or invalid tags

```sh
exifmeta check
```

```sh
exifmeta check "Photos/shoot-001"
```

### `write`

Write the EXIF tags from `metadata.yml` to images.

This writes to all images in the current directory:

```sh
exifmeta write metadata.yml
```

You can also write to one image:

```sh
exifmeta write metadata.yml "10.tif"
```

### `read`

Read and print the EXIF tags from one image.

```sh
exifmeta read "10.tif"
```

### `strip`

Remove EXIF tags from images.

This removes EXIF from all images in the current directory:

```sh
exifmeta strip
```

You can also keep/remove specific EXIF tags, but keep a select few:

```sh
exifmeta strip --keep "Make,Model,LensMake,LensModel"
```

```sh
exifmeta strip --remove "GPSLatitude,GPSLongitude"
```

A special flag `--privacy` exists to only strip all identifiable tags (such as all GPS tags):

```sh
exifmeta strip --privacy
```

### `interactive`

Open an interactive browser for folders and images. It starts in read mode by default.

```sh
exifmeta interactive
```

You can also open a specific directory:

```sh
exifmeta interactive "Photos/shoot-001"
```

Press `w` to toggle write mode. In write mode, select an image and press `Enter`
or `Right` to edit writable tags. Press `a` to add a tag, then choose a
writable standard EXIF tag or enter a custom tag name. Each confirmed edit is
written immediately; with `--dry-run`, edits are simulated without changing
files. The standard picker only lists EXIF tags currently supported by
exifmeta's writer.

### `metadata.yml` file

This file is used to write EXIF tags, and can be used to either write to a single file or many at once (an entire directory of images).

An example `metadata.yml` file that sets the camera and lens make+model:

```yml
exif:
    # Camera & Lens
    Make: Zenza Bronica
    Model: ETRS
    LensMake: Zenza Bronica
    LensModel: Zenzanon 75mm f/2.8
    FocalLength: 75mm
    MaxApertureValue: 2.8
```

---

- [EXIF Tags reference](https://exiftool.org/TagNames/EXIF.html)
- [Locations reference](https://www.geonames.org)

See an [example `metadata.yml`](./examples/metadata.yml) file in [`./examples`](./examples/metadata.yml) directory.

---

<small>
    <a href="https://www.flaticon.com/free-icons/ui" title="ui icons">Ui icons created by smashingstocks - Flaticon</a>
</small>
