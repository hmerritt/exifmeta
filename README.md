# Exif Metadata

A simple program to read a standardised `metadata.yaml` file and add the information as exif to all image files in the same directory.

## CLI Commands

| Command    | Function                                                                                     |
| :--------- | :------------------------------------------------------------------------------------------- |
| `run`      | Main function; reads `metadata.yaml` file and adds information as exif to target image files |
| `init`     | Create template `metadata.yaml` file                                                         |
| `validate` | Checks `metadata.yaml` is valid                                                              |
| `inspect`  | Read and pretty-print the current EXIF data of a specific image file                         |
| `strip`    | Removes all existing EXIF metadata from target image files                                   |

### Flags

| Command          | Function                                                                                                                                   |
| :--------------- | :----------------------------------------------------------------------------------------------------------------------------------------- |
| `--dry-run`      | Runs the program in 'simulation' mode, without making any changes to any files                                                             |
| `--no-overwrite` | Prevents overwriting exif data if there is already data there                                                                              |
| `--extensions`   | Restricts processing to specified file typologies to prevent the script from attempting to modify unsupported binaries (e.g., -e jpg,tiff) |
| `--recursive`    | Find image files across all subdirectories, applying the root configuration to nested image repositories                                   |

### Supported Image File Formats

- JPEG / JPG
- JXL
- HEIF / HEIC / HIF / AVIF
- PNG
- TIFF
- WebP (only lossless and extended)

## `metadata.yaml` file

See example files in [`./examples`](./examples/metadata.yml) directory.

| Parent Interface                 | Property                 |
| :------------------------------- | :----------------------- |
| `roll`                           | `number`                 |
| `date`                           | `string` -> `YYYY-MM-DD` |
| `date_end`                       | `string` -> `YYYY-MM-DD` |
| `frame_count`                    | `number`                 |
| `notable_frames`                 | `number[]`               |
| `locations`                      | `string[]`               |
| `comment`                        | `string`                 |
| `by`                             | `string[]`               |
| `camera -> make`                 | `string`                 |
| `camera -> model`                | `string`                 |
| `lenses -> lens -> make`         | `string`                 |
| `lenses -> lens -> model`        | `string`                 |
| `lenses -> lens -> focal_length` | `string`                 |
| `film -> format`                 | `number`                 |
| `film -> stock`                  | `stock`                  |
| `film -> stock -> manufacturer`  | `string`                 |
| `film -> stock -> name`          | `string`                 |
| `film -> stock -> iso`           | `number`                 |
| `film -> stock -> color`         | `boolean`                |
| `film -> stock -> negative`      | `boolean`                |
| `development -> developer`       | `string`                 |
| `development -> process`         | `string`                 |
| `development -> developing_date` | `string`                 |
| `development -> developed_by`    | `string`                 |
| `scans -> scan -> lab`           | `boolean`                |
| `scans -> scan -> frames`        | `number[]`               |
| `scans -> scan -> scanner`       | `string`                 |
| `scans -> scan -> resolution`    | `string`                 |
| `exif`                           | `string[]`               |

---

`examples/metadata.yml`:

```yaml
roll: 35
date: 2026-04-26
date_end: 2026-04-26
frame_count: 15
notable_frames: [5, 9, 15]
locations: [Wales]
comment:

by:
    - Harry Merritt

camera:
    make: Zenza Bronica
    model: ETRS

lenses:
    - make: Zenza Bronica
      model: Zenzanon 75mm f/2.8
      focal_length: 75mm

film:
    format: 120
    stock:
        manufacturer: CineStill Kodak
        name: Kodak Double-X
        iso: 250
        color: false
        negative: true

development:
    developer:
    process: B&W
    developing_date: 2026-04-28
    developed_by: The Darkroom, UK

scans:
    - lab: true
      frames: []
      scanner: Noritsu
      resolution: 2796x2048

exif:
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

### Implementation

- [https://github.com/TechnikTobi/little_exif](TechnikTobi/little_exif) — A library for reading and writing EXIF data in pure Rust.
