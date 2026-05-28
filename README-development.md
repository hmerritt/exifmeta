<img src="./assets/icon.png" draggable="false" width="100px" />

# Exif Metadata

[![Release](https://img.shields.io/github/v/release/hmerritt/exifmeta?link=https%3A%2F%2Fgithub.com%2Fhmerritt%2Fexifmeta%2Freleases%2Flatest)](https://github.com/hmerritt/exifmeta/releases/latest) [![Downloads](https://img.shields.io/github/downloads/hmerritt/exifmeta/total?link=https%3A%2F%2Fgithub.com%2Fhmerritt%2Fexifmeta%2Freleases%2Flatest)](https://github.com/hmerritt/exifmeta/releases/latest) [![Coverage](https://img.shields.io/coverallsCoverage/github/hmerritt/exifmeta)](https://coveralls.io/github/hmerritt/exifmeta?branch=master)

EXIF tool for photographers.

A simple program to read a standardised `metadata.yml` file and write the data as EXIF to all image files in the same directory.

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

---

<small>
    <a href="https://www.flaticon.com/free-icons/ui" title="ui icons">Ui icons created by smashingstocks - Flaticon</a>
</small>
